//! Device-local book state that must NOT replicate across the account-sync mesh.
//!
//! `book_local` is a sibling regular (non-CRR) table keyed by the book `uuid`.
//! cr-sqlite replicates every non-PK column of a CRR (it has no per-column
//! opt-out), so any genuinely per-device fact about a book lives here, out of
//! the `books` CRR. See ADR-044. It holds two independent negative flags:
//!
//! - `hub_cover_upload_failed_at`: a timestamp of *this* device's last failed
//!   hub cover upload, which would produce false "upload failed" badges on
//!   other devices if it replicated.
//! - `cover_lookup_failed_at`: when the startup cover sweep last established
//!   that no external source carries a cover for this book. Device-local by
//!   necessity rather than by choice — the fact itself is about the world, not
//!   the device, but `books` is a CRR and cannot take a new column.
//!
//! This module is the single access point for the table, so the hub-upload
//! writer, the cover sweep and the owner-facing read path share one set of
//! statements.
//!
//! Because the two flags are independent, clearing one must never delete the
//! row: it nulls its own column and drops the row only once every column is
//! null, so the table keeps holding one row per *pending* fact and no more.

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

/// Clear the pending hub-upload-failure flag for one book, leaving any other
/// device-local fact about it (the cover-lookup marker) untouched.
pub async fn clear_cover_upload_failed_at(
    db: &DatabaseConnection,
    book_uuid: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE book_local SET hub_cover_upload_failed_at = NULL WHERE book_uuid = ?",
        [book_uuid.into()],
    ))
    .await?;
    prune_empty_rows(db).await
}

/// Clear every pending hub-upload-failure flag (called when the library
/// unregisters from the hub, so stale badges do not survive a purge /
/// re-registration cycle). Cover-lookup markers survive: they say nothing about
/// the hub.
pub async fn clear_all_cover_upload_failures(db: &DatabaseConnection) -> Result<(), DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "UPDATE book_local SET hub_cover_upload_failed_at = NULL".to_owned(),
    ))
    .await?;
    prune_empty_rows(db).await
}

/// Drop rows that no longer carry any pending fact.
///
/// `pending_cover_upload_failures` scans the whole table on every list page, so
/// the table must not accumulate rows whose every column is null.
async fn prune_empty_rows(db: &DatabaseConnection) -> Result<(), DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM book_local \
         WHERE hub_cover_upload_failed_at IS NULL AND cover_lookup_failed_at IS NULL"
            .to_owned(),
    ))
    .await?;
    Ok(())
}

/// Record that a silent cover lookup for `book_uuid` came back empty *and*
/// every source it asked actually answered, so the absence is a fact rather
/// than an unknown.
///
/// Only conclusive absences reach this function: see
/// `services::book_service::enrich_missing_covers`. An outage recorded here
/// would suppress retries for a book whose cover exists.
pub async fn set_cover_lookup_failed_at(
    db: &DatabaseConnection,
    book_uuid: &str,
    when: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO book_local (book_uuid, cover_lookup_failed_at) VALUES (?, ?) \
         ON CONFLICT(book_uuid) DO UPDATE SET \
         cover_lookup_failed_at = excluded.cover_lookup_failed_at",
        [book_uuid.into(), when.into()],
    ))
    .await?;
    Ok(())
}

/// Forget that a cover lookup ever came back empty for one book, so the next
/// sweep asks the sources again. Called when a cover is found by any route.
pub async fn clear_cover_lookup_failed_at(
    db: &DatabaseConnection,
    book_uuid: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE book_local SET cover_lookup_failed_at = NULL WHERE book_uuid = ?",
        [book_uuid.into()],
    ))
    .await?;
    prune_empty_rows(db).await
}

/// Every book whose cover lookup last came back conclusively empty, as
/// `book_uuid -> failed_at`.
///
/// Read once per sweep and looked up in memory: the alternative is one query
/// per coverless book, and the sweep already walks them all.
pub async fn cover_lookup_failures(
    db: &DatabaseConnection,
) -> Result<HashMap<String, String>, DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT book_uuid, cover_lookup_failed_at FROM book_local \
             WHERE cover_lookup_failed_at IS NOT NULL"
                .to_owned(),
        ))
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let uuid: String = r.try_get("", "book_uuid")?;
        if let Some(ts) = r.try_get::<Option<String>>("", "cover_lookup_failed_at")? {
            out.insert(uuid, ts);
        }
    }
    Ok(out)
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
/// `book_local` only ever holds rows carrying a pending fact (every column is
/// nulled on clear and the empty row pruned), so this scans a small table with
/// no parameters, rather than binding a whole page of ids into an `IN (...)`
/// (which would also hit SQLite's bound-parameter ceiling on large libraries).
/// The `WHERE` is what keeps it correct now that a row may exist for a
/// cover-lookup marker alone.
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
    async fn the_two_flags_are_independent() {
        let db = setup().await;

        set_cover_upload_failed_at(&db, "book-1", "2026-08-26T10:00:00Z")
            .await
            .unwrap();
        set_cover_lookup_failed_at(&db, "book-1", "2026-08-26T11:00:00Z")
            .await
            .unwrap();

        // Clearing one leaves the other, and the shared row survives.
        clear_cover_upload_failed_at(&db, "book-1").await.unwrap();
        assert_eq!(cover_upload_failed_at(&db, "book-1").await.unwrap(), None);
        assert_eq!(
            cover_lookup_failures(&db).await.unwrap().get("book-1"),
            Some(&"2026-08-26T11:00:00Z".to_string())
        );

        // And clearing the last one drops the row.
        clear_cover_lookup_failed_at(&db, "book-1").await.unwrap();
        assert!(cover_lookup_failures(&db).await.unwrap().is_empty());
        assert!(pending_cover_upload_failures(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unregistering_from_the_hub_keeps_cover_lookup_markers() {
        let db = setup().await;
        set_cover_upload_failed_at(&db, "book-1", "2026-08-26T10:00:00Z")
            .await
            .unwrap();
        set_cover_lookup_failed_at(&db, "book-1", "2026-08-26T11:00:00Z")
            .await
            .unwrap();
        set_cover_lookup_failed_at(&db, "book-2", "2026-08-26T11:00:00Z")
            .await
            .unwrap();

        clear_all_cover_upload_failures(&db).await.unwrap();

        // The hub flags are gone; what no source carries is not a hub fact.
        assert!(pending_cover_upload_failures(&db).await.unwrap().is_empty());
        assert_eq!(cover_lookup_failures(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cover_lookup_marker_roundtrip() {
        let db = setup().await;
        assert!(cover_lookup_failures(&db).await.unwrap().is_empty());

        set_cover_lookup_failed_at(&db, "book-1", "2026-08-26T10:00:00Z")
            .await
            .unwrap();
        set_cover_lookup_failed_at(&db, "book-1", "2026-08-26T12:00:00Z")
            .await
            .unwrap();

        let map = cover_lookup_failures(&db).await.unwrap();
        assert_eq!(map.len(), 1, "upsert refreshes rather than duplicating");
        assert_eq!(
            map.get("book-1").map(String::as_str),
            Some("2026-08-26T12:00:00Z")
        );

        // A book that never failed carries no marker at all.
        assert!(!map.contains_key("book-2"));
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
