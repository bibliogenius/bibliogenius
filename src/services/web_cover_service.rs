//! Remember where a cover picked from a web page came from.
//!
//! Once saved, a web cover is an ordinary local cover file (`<uuid>.jpg`) that
//! travels like a photographed one. What only this device knows is the image's
//! web address, wanted later to hand the cover to Open Library as a URL rather
//! than as our copy. The address is stored in `book_local` with the SHA-256 of
//! the file it describes: replacing the cover (a photo, another pick) rewrites
//! the same file name, so the digest is what tells a current origin from a
//! stale one, without every cover-writing path having to remember to clear it.

use std::path::Path;

use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};

use crate::infrastructure::book_local::{self, CoverOrigin};
use crate::utils::cover_url::own_local_cover_path;
use crate::utils::public_fetch::parse_public_https_url;

/// Record that the cover file currently on disk for `book_id` came from
/// `image_url`. Call it right after saving the file.
pub async fn record_cover_origin(
    db: &DatabaseConnection,
    covers_dir: &Path,
    book_id: &str,
    image_url: &str,
) -> Result<(), String> {
    let url = parse_public_https_url(image_url).map_err(|_| "invalid_url".to_string())?;
    let path = own_local_cover_path(covers_dir, book_id).ok_or("invalid_book_id")?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| "cover_file_missing".to_string())?;
    let origin = CoverOrigin {
        url: url.to_string(),
        sha256: sha256_hex(&bytes),
    };
    book_local::set_cover_origin(db, book_id, &origin)
        .await
        .map_err(|e| e.to_string())
}

/// The web address of the cover currently on disk for `book_id`, or `None`
/// when none was recorded, the file is gone, or the file has been replaced
/// since (the digest no longer matches; the stale record is then dropped).
pub async fn current_cover_origin_url(
    db: &DatabaseConnection,
    covers_dir: &Path,
    book_id: &str,
) -> Result<Option<String>, String> {
    let Some(origin) = book_local::cover_origin(db, book_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let Some(path) = own_local_cover_path(covers_dir, book_id) else {
        return Ok(None);
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return Ok(None);
    };
    if sha256_hex(&bytes) == origin.sha256 {
        return Ok(Some(origin.url));
    }
    book_local::clear_cover_origin(db, book_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(None)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;

    async fn setup() -> (DatabaseConnection, tempfile::TempDir) {
        let db = init_db("sqlite::memory:").await.expect("init db");
        (db, tempfile::tempdir().expect("tempdir"))
    }

    const URL: &str = "https://cdn.editions-exemple.fr/covers/42.jpg";

    #[tokio::test]
    async fn the_origin_holds_while_the_file_is_unchanged() {
        let (db, dir) = setup().await;
        std::fs::write(dir.path().join("book-1.jpg"), b"web cover bytes").unwrap();

        record_cover_origin(&db, dir.path(), "book-1", URL)
            .await
            .unwrap();
        assert_eq!(
            current_cover_origin_url(&db, dir.path(), "book-1")
                .await
                .unwrap(),
            Some(URL.to_string())
        );
    }

    #[tokio::test]
    async fn replacing_the_cover_invalidates_the_origin() {
        let (db, dir) = setup().await;
        let file = dir.path().join("book-1.jpg");
        std::fs::write(&file, b"web cover bytes").unwrap();
        record_cover_origin(&db, dir.path(), "book-1", URL)
            .await
            .unwrap();

        // A photo replaces the cover under the same file name.
        std::fs::write(&file, b"photo bytes").unwrap();
        assert_eq!(
            current_cover_origin_url(&db, dir.path(), "book-1")
                .await
                .unwrap(),
            None
        );
        // The stale record is gone, not merely hidden.
        assert_eq!(book_local::cover_origin(&db, "book-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_missing_file_has_no_origin() {
        let (db, dir) = setup().await;
        std::fs::write(dir.path().join("book-1.jpg"), b"x").unwrap();
        record_cover_origin(&db, dir.path(), "book-1", URL)
            .await
            .unwrap();
        std::fs::remove_file(dir.path().join("book-1.jpg")).unwrap();
        assert_eq!(
            current_cover_origin_url(&db, dir.path(), "book-1")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn refuses_to_record_without_a_file_or_with_a_bad_url() {
        let (db, dir) = setup().await;
        assert_eq!(
            record_cover_origin(&db, dir.path(), "book-1", URL).await,
            Err("cover_file_missing".to_string())
        );
        std::fs::write(dir.path().join("book-1.jpg"), b"x").unwrap();
        assert_eq!(
            record_cover_origin(&db, dir.path(), "book-1", "http://example.org/a.jpg").await,
            Err("invalid_url".to_string())
        );
        assert_eq!(
            record_cover_origin(&db, dir.path(), "../etc", URL).await,
            Err("invalid_book_id".to_string())
        );
        assert_eq!(book_local::cover_origin(&db, "book-1").await.unwrap(), None);
    }
}
