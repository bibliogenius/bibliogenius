//! File-backed custom-cover transport for account sync (ADR-046).
//!
//! cr-sqlite replicates the `books` row, including the normalized `cover_url`,
//! but not the cover file itself. A custom cover (a photo the user took, stored
//! as `<uuid>.jpg` in the `covers/` directory) only exists on the device that
//! captured it, so a secondary device shows a placeholder until the bytes arrive.
//!
//! This module is the production [`CoverSource`]/[`CoverSink`] pair wired into the
//! account-sync cycle: the source reads this device's local covers and re-encodes
//! each to fit a lane blob; the sink writes a received cover under the book's uuid
//! so the existing resolver finds it once the already-replicated `cover_url` row
//! lands. The covers directory is injected by the caller (the FFI entrypoint),
//! keeping this service free of any dependency on the `api` layer.

use std::path::PathBuf;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect};

use crate::models::book;
use crate::services::account_sync_engine::{CoverSink, CoverSource, OutboundCover, SyncError};
use crate::utils::cover_image;
use crate::utils::cover_url;
use async_trait::async_trait;

/// True when `uuid` is safe to use as a single path component: no separators, no
/// `..`, non-empty. A cover lane is authored by one of the user's own authorized
/// devices, but the sink writes to disk from network-delivered data, so we never
/// let the uuid escape the covers directory (defense in depth).
fn is_safe_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && uuid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Reads this device's local custom covers and re-encodes them for transport.
pub struct DbCoverSource {
    db: DatabaseConnection,
    covers_dir: PathBuf,
}

impl DbCoverSource {
    pub fn new(db: DatabaseConnection, covers_dir: PathBuf) -> Self {
        Self { db, covers_dir }
    }
}

#[async_trait]
impl CoverSource for DbCoverSource {
    async fn pending_covers(&self) -> Result<Vec<OutboundCover>, SyncError> {
        // Pull just the identity + cover_url of every book with a cover, then keep
        // the device-local ones (a bare `<uuid>.jpg`, not an http/`/api` value).
        let rows: Vec<(String, Option<String>)> = book::Entity::find()
            .select_only()
            .column(book::Column::Id)
            .column(book::Column::CoverUrl)
            .filter(book::Column::CoverUrl.is_not_null())
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| SyncError::State(format!("scan covers: {e}")))?;

        let covers_dir = self.covers_dir.clone();
        // File reads + JPEG re-encodes are blocking and CPU-bound; keep them off the
        // async runtime (the account-sync build runs a single-connection pool).
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for (uuid, cover_url_val) in rows {
                let Some(stored) = cover_url_val else { continue };
                if !cover_url::is_local_cover(&stored) || !is_safe_uuid(&uuid) {
                    continue;
                }
                let path = covers_dir.join(cover_url::local_cover_filename(&uuid));
                let Ok(meta) = std::fs::metadata(&path) else {
                    // cover_url says local but the file is absent (e.g. not yet
                    // received from the originating device): nothing to push.
                    continue;
                };
                // Freshness clock: the file mtime advances when the user replaces
                // the photo, so the receiver rewrites only on a real change.
                let hlc = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let raw = match std::fs::read(&path) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(book = %uuid, error = %e, "failed to read custom cover");
                        continue;
                    }
                };
                match cover_image::resize_to_jpeg_thumbnail_for_sync(&raw) {
                    Ok(Some(bytes)) => out.push(OutboundCover {
                        book_uuid: uuid,
                        bytes,
                        hlc,
                    }),
                    Ok(None) => tracing::warn!(
                        book = %uuid,
                        "custom cover too large to fit a sync lane after re-encode; peer keeps a placeholder"
                    ),
                    Err(e) => {
                        tracing::warn!(book = %uuid, error = %e, "failed to re-encode custom cover for sync")
                    }
                }
            }
            out
        })
        .await
        .map_err(|e| SyncError::State(format!("cover scan task: {e}")))
    }
}

/// Writes a custom cover received from another device under the book's uuid.
pub struct FsCoverSink {
    covers_dir: PathBuf,
}

impl FsCoverSink {
    pub fn new(covers_dir: PathBuf) -> Self {
        Self { covers_dir }
    }
}

#[async_trait]
impl CoverSink for FsCoverSink {
    async fn write_cover(&self, book_uuid: &str, bytes: &[u8], _hlc: i64) -> Result<(), SyncError> {
        if !is_safe_uuid(book_uuid) {
            // Never let a malformed uuid from a lane escape the covers directory.
            tracing::warn!(book = %book_uuid, "skipping cover with an unsafe uuid");
            return Ok(());
        }
        let dir = self.covers_dir.clone();
        let uuid = book_uuid.to_string();
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir)?;
            let final_path = dir.join(cover_url::local_cover_filename(&uuid));
            // Write a temp sibling then rename over the target so a reader never
            // observes a half-written cover.
            let tmp_path = dir.join(format!("{uuid}.jpg.tmp"));
            std::fs::write(&tmp_path, &bytes)?;
            std::fs::rename(&tmp_path, &final_path)?;
            Ok(())
        })
        .await
        .map_err(|e| SyncError::State(format!("cover write task: {e}")))?
        .map_err(|e| SyncError::State(format!("write cover {book_uuid}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_uuids() {
        assert!(is_safe_uuid("0190f5a2-1234-7abc-8def-0123456789ab"));
        assert!(is_safe_uuid("book_1"));
        assert!(!is_safe_uuid(""));
        assert!(!is_safe_uuid("../etc/passwd"));
        assert!(!is_safe_uuid("a/b"));
        assert!(!is_safe_uuid("a.b"));
    }

    #[tokio::test]
    async fn sink_writes_cover_under_uuid_filename() {
        let dir = std::env::temp_dir().join(format!("bg_cover_sink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = FsCoverSink::new(dir.clone());

        sink.write_cover("test-uuid-1", b"JPEGDATA", 42)
            .await
            .unwrap();

        let written = std::fs::read(dir.join("test-uuid-1.jpg")).unwrap();
        assert_eq!(written, b"JPEGDATA");
        // No temp file left behind.
        assert!(!dir.join("test-uuid-1.jpg.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sink_skips_unsafe_uuid_without_writing() {
        let dir = std::env::temp_dir().join(format!("bg_cover_sink_unsafe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sink = FsCoverSink::new(dir.clone());

        // Must not create anything and must not error the sync cycle.
        sink.write_cover("../escape", b"x", 1).await.unwrap();
        assert!(!dir.join("../escape.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
