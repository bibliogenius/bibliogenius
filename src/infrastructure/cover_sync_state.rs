//! Device-local bookkeeping of which custom covers have already been pushed to
//! (or received from) the account-sync mesh, so the periodic auto-sync does not
//! re-encode and re-upload every cover on every cycle (ADR-046).
//!
//! `cover_sync_state` is a sibling regular (non-CRR) table keyed by the book
//! `uuid`, holding the file modification time (`file_mtime`, epoch seconds) that
//! was last synced for that cover. It must stay LOCAL: it records what *this*
//! device has transported, which differs per device, so replicating it would be
//! meaningless. Like [`super::book_local`], it lives out of the `books` CRR.
//!
//! Why mtime and not a content hash: the cover file is the freshness clock
//! already (the producer side reads it as the lane HLC), and re-hashing every
//! cover each cycle would defeat the CPU saving this table exists to provide. A
//! restore that resets file mtimes causes at most a one-time, idempotent re-push.
//!
//! Two writers share this table:
//! - the producer marks a cover synced after its bytes push successfully, so the
//!   next scan skips it while its mtime is unchanged;
//! - the receiver records a cover it just wrote from another device, so this
//!   device never bounces that same cover back (the A→B→A echo).

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};

/// Every book uuid that has a recorded last-synced cover mtime, as
/// `book_uuid -> file_mtime` (epoch seconds). The producer loads this once per
/// cycle and skips any cover whose current file mtime equals the stored value.
///
/// The table only ever holds rows for books with a synced custom cover, so this
/// scans a tiny table with no parameters rather than binding a page of ids.
pub async fn synced_mtimes(db: &DatabaseConnection) -> Result<HashMap<String, i64>, DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT book_uuid, file_mtime FROM cover_sync_state".to_owned(),
        ))
        .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let uuid: String = r.try_get("", "book_uuid")?;
        let mtime: i64 = r.try_get("", "file_mtime")?;
        out.insert(uuid, mtime);
    }
    Ok(out)
}

/// Record that the cover for `book_uuid` is synced at `file_mtime` (epoch
/// seconds). Upsert: a later edit (new mtime) overwrites the stored value, so the
/// cover is re-pushed exactly once per real change.
pub async fn mark_synced(
    db: &DatabaseConnection,
    book_uuid: &str,
    file_mtime: i64,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO cover_sync_state (book_uuid, file_mtime) VALUES (?, ?) \
         ON CONFLICT(book_uuid) DO UPDATE SET file_mtime = excluded.file_mtime",
        [book_uuid.into(), file_mtime.into()],
    ))
    .await?;
    Ok(())
}

/// Batched [`mark_synced`] for the producer's after-push recording: a single
/// multi-row upsert instead of one statement per cover. The first sync of a large
/// library marks every cover at once, so this avoids N round-trips on the pinned
/// single-connection pool. Chunked to stay under SQLite's bound-parameter ceiling
/// (2 params/row). A no-op on an empty slice.
pub async fn mark_synced_many(
    db: &DatabaseConnection,
    covers: &[(String, i64)],
) -> Result<(), DbErr> {
    // SQLite's default SQLITE_MAX_VARIABLE_NUMBER is 999; at 2 params/row, 400
    // rows (800 params) stays comfortably under it across builds.
    const ROWS_PER_BATCH: usize = 400;
    let backend = db.get_database_backend();
    for chunk in covers.chunks(ROWS_PER_BATCH) {
        let placeholders = vec!["(?, ?)"; chunk.len()].join(", ");
        let sql = format!(
            "INSERT INTO cover_sync_state (book_uuid, file_mtime) VALUES {placeholders} \
             ON CONFLICT(book_uuid) DO UPDATE SET file_mtime = excluded.file_mtime"
        );
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(chunk.len() * 2);
        for (uuid, mtime) in chunk {
            values.push(uuid.as_str().into());
            values.push((*mtime).into());
        }
        db.execute(Statement::from_sql_and_values(backend, sql, values))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;

    async fn setup() -> DatabaseConnection {
        init_db("sqlite::memory:").await.expect("init db")
    }

    #[tokio::test]
    async fn mark_then_load_roundtrips() {
        let db = setup().await;
        assert!(synced_mtimes(&db).await.unwrap().is_empty());

        mark_synced(&db, "book-1", 1000).await.unwrap();
        mark_synced(&db, "book-2", 2000).await.unwrap();

        let map = synced_mtimes(&db).await.unwrap();
        assert_eq!(map.get("book-1"), Some(&1000));
        assert_eq!(map.get("book-2"), Some(&2000));
        assert_eq!(map.get("book-3"), None);
    }

    #[tokio::test]
    async fn mark_upserts_on_a_newer_mtime() {
        let db = setup().await;
        mark_synced(&db, "book-1", 1000).await.unwrap();
        mark_synced(&db, "book-1", 1500).await.unwrap();

        let map = synced_mtimes(&db).await.unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("book-1"), Some(&1500));
    }

    #[tokio::test]
    async fn mark_many_upserts_in_one_pass() {
        let db = setup().await;
        // Empty slice is a no-op.
        mark_synced_many(&db, &[]).await.unwrap();
        assert!(synced_mtimes(&db).await.unwrap().is_empty());

        // Seed one row, then a batch that inserts two new rows AND updates the seed.
        mark_synced(&db, "book-1", 100).await.unwrap();
        mark_synced_many(
            &db,
            &[
                ("book-1".to_string(), 999), // update
                ("book-2".to_string(), 200), // insert
                ("book-3".to_string(), 300), // insert
            ],
        )
        .await
        .unwrap();

        let map = synced_mtimes(&db).await.unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("book-1"), Some(&999));
        assert_eq!(map.get("book-2"), Some(&200));
        assert_eq!(map.get("book-3"), Some(&300));
    }

    #[tokio::test]
    async fn mark_many_handles_more_than_one_chunk() {
        let db = setup().await;
        // 850 rows spans two 400-row chunks (and a partial third), exercising the
        // chunking loop and staying well under the bound-parameter ceiling.
        let covers: Vec<(String, i64)> =
            (0..850).map(|i| (format!("book-{i}"), i as i64)).collect();
        mark_synced_many(&db, &covers).await.unwrap();

        let map = synced_mtimes(&db).await.unwrap();
        assert_eq!(map.len(), 850);
        assert_eq!(map.get("book-0"), Some(&0));
        assert_eq!(map.get("book-849"), Some(&849));
    }
}
