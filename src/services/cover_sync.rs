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

use crate::infrastructure::cover_sync_state;
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

    /// Record that the given `(book_uuid, file_mtime)` covers pushed successfully,
    /// so a later [`Self::pending_covers`] skips re-encoding and re-uploading them
    /// while unchanged (ADR-046 dedup). The caller invokes this only after the
    /// whole sync cycle succeeds (mirroring the row-push watermark): a failed push
    /// leaves the covers un-recorded so they are retried next cycle.
    pub async fn mark_pushed(&self, pushed: &[(String, i64)]) -> Result<(), SyncError> {
        cover_sync_state::mark_synced_many(&self.db, pushed)
            .await
            .map_err(|e| SyncError::State(format!("record {} pushed covers: {e}", pushed.len())))
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

        // Dedup state: the last mtime we already pushed (or received) per book. A
        // cover whose file mtime is unchanged is skipped BEFORE the costly re-read
        // and re-encode, so the periodic auto-sync does not churn every cover each
        // cycle (ADR-046). Loaded once here on the async side, then moved into the
        // blocking scan.
        let synced = cover_sync_state::synced_mtimes(&self.db)
            .await
            .map_err(|e| SyncError::State(format!("load cover dedup state: {e}")))?;

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
                // Already synced at this exact mtime: skip the re-read/re-encode.
                if synced.get(&uuid) == Some(&hlc) {
                    continue;
                }
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
    db: DatabaseConnection,
    covers_dir: PathBuf,
}

impl FsCoverSink {
    pub fn new(db: DatabaseConnection, covers_dir: PathBuf) -> Self {
        Self { db, covers_dir }
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
        // Stat the file we just wrote so the dedup state records its ACTUAL local
        // mtime (the receiver's write time, not the sender's): this is what the next
        // `pending_covers` scan reads, so recording it prevents this device from
        // bouncing the cover straight back to the sender (the A->B->A echo).
        let written_mtime = tokio::task::spawn_blocking(move || -> std::io::Result<i64> {
            std::fs::create_dir_all(&dir)?;
            let final_path = dir.join(cover_url::local_cover_filename(&uuid));
            // Write a temp sibling then rename over the target so a reader never
            // observes a half-written cover.
            let tmp_path = dir.join(format!("{uuid}.jpg.tmp"));
            std::fs::write(&tmp_path, &bytes)?;
            std::fs::rename(&tmp_path, &final_path)?;
            let mtime = std::fs::metadata(&final_path)?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Ok(mtime)
        })
        .await
        .map_err(|e| SyncError::State(format!("cover write task: {e}")))?
        .map_err(|e| SyncError::State(format!("write cover {book_uuid}: {e}")))?;
        cover_sync_state::mark_synced(&self.db, book_uuid, written_mtime)
            .await
            .map_err(|e| SyncError::State(format!("record received cover {book_uuid}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;

    async fn db() -> DatabaseConnection {
        init_db("sqlite::memory:").await.expect("init db")
    }

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
        let db = db().await;
        let dir = std::env::temp_dir().join(format!("bg_cover_sink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = FsCoverSink::new(db.clone(), dir.clone());

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
    async fn sink_records_received_cover_in_dedup_state() {
        // A received cover must be recorded so this device never bounces it back
        // to the sender (the A->B->A echo).
        let db = db().await;
        let dir = std::env::temp_dir().join(format!("bg_cover_echo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sink = FsCoverSink::new(db.clone(), dir.clone());

        sink.write_cover("book-echo", b"JPEGDATA", 99)
            .await
            .unwrap();

        let synced = cover_sync_state::synced_mtimes(&db).await.unwrap();
        let written_mtime = std::fs::metadata(dir.join("book-echo.jpg"))
            .unwrap()
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap();
        // Recorded under the local write mtime (not the sender's hlc of 99), which
        // is exactly what `pending_covers` will read back and skip on.
        assert_eq!(synced.get("book-echo"), Some(&written_mtime));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sink_skips_unsafe_uuid_without_writing() {
        let db = db().await;
        let dir = std::env::temp_dir().join(format!("bg_cover_sink_unsafe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sink = FsCoverSink::new(db.clone(), dir.clone());

        // Must not create anything and must not error the sync cycle.
        sink.write_cover("../escape", b"x", 1).await.unwrap();
        assert!(!dir.join("../escape.jpg").exists());
        // And nothing recorded for an unsafe uuid.
        assert!(
            cover_sync_state::synced_mtimes(&db)
                .await
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn source_skips_a_cover_already_synced_at_the_same_mtime() {
        let db = db().await;
        let dir = std::env::temp_dir().join(format!("bg_cover_dedup_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A book whose cover is a local custom photo `<uuid>.jpg`.
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};
        let uuid = "book-dedup-1";
        book::ActiveModel {
            id: Set(uuid.to_string()),
            title: Set("T".to_string()),
            reading_status: Set("to_read".to_string()),
            owned: Set(true),
            private: Set(false),
            created_at: Set("2026-06-30T00:00:00Z".to_string()),
            updated_at: Set("2026-06-30T00:00:00Z".to_string()),
            cover_url: Set(Some(format!("{uuid}.jpg"))),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        // A real (decodable) JPEG so the re-encode step keeps it; the dedup logic
        // under test runs only on covers that survive re-encoding.
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            120,
            180,
            image::Rgb([10, 20, 30]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        std::fs::write(dir.join(format!("{uuid}.jpg")), &jpeg).unwrap();

        let source = DbCoverSource::new(db.clone(), dir.clone());

        // First scan: the cover is new, so it is produced for push.
        let first = source.pending_covers().await.unwrap();
        assert_eq!(first.len(), 1, "new cover should be pushed");
        let mtime = first[0].hlc;

        // Simulate a successful push recording the dedup state.
        source
            .mark_pushed(&[(uuid.to_string(), mtime)])
            .await
            .unwrap();

        // Second scan, file unchanged: skipped (no re-encode, no push).
        let second = source.pending_covers().await.unwrap();
        assert!(second.is_empty(), "unchanged cover must be skipped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
