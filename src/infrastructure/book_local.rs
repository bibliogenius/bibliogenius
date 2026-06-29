//! Device-local book state that must NOT replicate across the account-sync mesh.
//!
//! `book_local` is a sibling regular (non-CRR) table keyed by the book `uuid`.
//! cr-sqlite replicates every non-PK column of a CRR (it has no per-column
//! opt-out), so any genuinely per-device fact about a book lives here, out of
//! the `books` CRR. Today that is exactly one column: the negative hub-cover
//! upload retry flag (`hub_cover_upload_failed_at`) — a timestamp of *this*
//! device's last failed cover upload, which would produce false "upload failed"
//! badges on other devices if it replicated. See ADR-044.
//!
//! This module is the single access point for the table, so the hub-upload
//! writer and the owner-facing read path share one set of statements.

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};

/// Record (or refresh) the timestamp of this device's last failed hub-cover
/// upload for `book_uuid`.
pub async fn set_cover_upload_failed_at(
    db: &DatabaseConnection,
    book_uuid: &str,
    when: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO book_local (book_uuid, hub_cover_upload_failed_at) VALUES (?, ?) \
         ON CONFLICT(book_uuid) DO UPDATE SET \
         hub_cover_upload_failed_at = excluded.hub_cover_upload_failed_at",
        [book_uuid.into(), when.into()],
    ))
    .await?;
    Ok(())
}

/// Clear the pending-failure flag for one book. `book_local` currently holds
/// only this flag, so clearing it removes the row; if the table later gains
/// other device-local columns this must become a targeted `UPDATE ... = NULL`.
pub async fn clear_cover_upload_failed_at(
    db: &DatabaseConnection,
    book_uuid: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "DELETE FROM book_local WHERE book_uuid = ?",
        [book_uuid.into()],
    ))
    .await?;
    Ok(())
}

/// Clear every pending-failure flag (called when the library unregisters from
/// the hub, so stale badges do not survive a purge / re-registration cycle).
pub async fn clear_all_cover_upload_failures(db: &DatabaseConnection) -> Result<(), DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM book_local".to_owned(),
    ))
    .await?;
    Ok(())
}

/// The pending hub-cover-upload-failure timestamp for one book, if any.
pub async fn cover_upload_failed_at(
    db: &DatabaseConnection,
    book_uuid: &str,
) -> Result<Option<String>, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT hub_cover_upload_failed_at FROM book_local WHERE book_uuid = ?",
            [book_uuid.into()],
        ))
        .await?;
    match row {
        Some(r) => r.try_get::<Option<String>>("", "hub_cover_upload_failed_at"),
        None => Ok(None),
    }
}

/// Every book with a pending hub-cover-upload failure, as `book_uuid ->
/// failed_at`. For list endpoints: callers look up their page's ids in the
/// returned map.
///
/// `book_local` only ever holds rows for books with a pending failure (set on
/// failure, removed on clear), so this scans a tiny table with no parameters,
/// rather than binding a whole page of ids into an `IN (...)` (which would also
/// hit SQLite's bound-parameter ceiling on large libraries).
pub async fn pending_cover_upload_failures(
    db: &DatabaseConnection,
) -> Result<HashMap<String, String>, DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT book_uuid, hub_cover_upload_failed_at FROM book_local \
             WHERE hub_cover_upload_failed_at IS NOT NULL"
                .to_owned(),
        ))
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let uuid: String = r.try_get("", "book_uuid")?;
        if let Some(ts) = r.try_get::<Option<String>>("", "hub_cover_upload_failed_at")? {
            out.insert(uuid, ts);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;

    async fn setup() -> DatabaseConnection {
        init_db("sqlite::memory:").await.expect("init db")
    }

    #[tokio::test]
    async fn set_get_clear_roundtrip() {
        let db = setup().await;
        assert_eq!(cover_upload_failed_at(&db, "book-1").await.unwrap(), None);

        set_cover_upload_failed_at(&db, "book-1", "2026-06-29T10:00:00Z")
            .await
            .unwrap();
        assert_eq!(
            cover_upload_failed_at(&db, "book-1").await.unwrap(),
            Some("2026-06-29T10:00:00Z".to_string())
        );

        // Upsert refreshes the timestamp rather than inserting a duplicate.
        set_cover_upload_failed_at(&db, "book-1", "2026-06-29T11:00:00Z")
            .await
            .unwrap();
        assert_eq!(
            cover_upload_failed_at(&db, "book-1").await.unwrap(),
            Some("2026-06-29T11:00:00Z".to_string())
        );

        clear_cover_upload_failed_at(&db, "book-1").await.unwrap();
        assert_eq!(cover_upload_failed_at(&db, "book-1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn pending_map_returns_only_flagged_books() {
        let db = setup().await;
        assert!(pending_cover_upload_failures(&db).await.unwrap().is_empty());

        set_cover_upload_failed_at(&db, "book-1", "2026-06-29T10:00:00Z")
            .await
            .unwrap();
        set_cover_upload_failed_at(&db, "book-3", "2026-06-29T12:00:00Z")
            .await
            .unwrap();

        let map = pending_cover_upload_failures(&db).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("book-1").map(String::as_str),
            Some("2026-06-29T10:00:00Z")
        );
        assert!(!map.contains_key("book-2"));
        assert_eq!(
            map.get("book-3").map(String::as_str),
            Some("2026-06-29T12:00:00Z")
        );

        // A cleared book drops out of the pending set.
        clear_cover_upload_failed_at(&db, "book-1").await.unwrap();
        let map = pending_cover_upload_failures(&db).await.unwrap();
        assert_eq!(map.len(), 1);
        assert!(!map.contains_key("book-1"));
    }

    #[tokio::test]
    async fn clear_all_wipes_every_flag() {
        let db = setup().await;
        set_cover_upload_failed_at(&db, "book-1", "t")
            .await
            .unwrap();
        set_cover_upload_failed_at(&db, "book-2", "t")
            .await
            .unwrap();
        clear_all_cover_upload_failures(&db).await.unwrap();
        assert_eq!(cover_upload_failed_at(&db, "book-1").await.unwrap(), None);
        assert_eq!(cover_upload_failed_at(&db, "book-2").await.unwrap(), None);
    }
}
