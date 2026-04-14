//! Cursor-paginated delta service over `operation_log`.
//!
//! Reads the local-source rows of a given `entity_type` after a client
//! cursor, collapses INSERT/UPDATE/DELETE into a single per-entity outcome,
//! and reports a fresh cursor plus `has_more` / `reset_required` flags.
//!
//! Designed as the single shared helper backing every per-entity delta
//! endpoint (ADR-028 D3). Privacy and entity-state resolution live in the
//! HTTP wrapper, not here.

use crate::models::operation_log;
use sea_orm::*;
use std::collections::HashMap;

/// Window of collapsed operations returned to a single delta pull.
#[derive(Debug, Clone)]
pub struct DeltaWindow {
    /// Collapsed operations, ordered by `id` ascending. Each entry maps to
    /// a single `upsert` or `delete` outcome for one entity.
    pub operations: Vec<DeltaOperation>,
    /// Cursor the client should persist to resume next pull. Equal to the
    /// id of the last raw row covered by this window (NOT the global max
    /// when `has_more` is true).
    pub latest_cursor: i64,
    /// True when the response is capped by `limit`. The client should
    /// re-query immediately with the new `latest_cursor`.
    pub has_more: bool,
    /// True when the cursor predates the oldest retained row. The HTTP
    /// wrapper must surface this as `410 Gone` so the client falls back
    /// to a full GET.
    pub reset_required: bool,
}

/// Post-collapse representation of a single entity's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaOperation {
    /// The id of the latest raw `operation_log` row that produced this
    /// outcome. Used by the wrapper as the per-row ordering key.
    pub id: i64,
    pub entity_type: String,
    pub entity_id: i32,
    /// Either `"upsert"` or `"delete"`.
    pub op: DeltaOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOp {
    Upsert,
    Delete,
}

/// Read a delta window for `entity_type` after the given cursor.
///
/// `since` is the last cursor the client successfully applied. `None` is
/// equivalent to "from the very beginning". `limit` caps the number of
/// raw rows scanned (collapsed output may be smaller).
///
/// Filters to `source = "local"` (D1): peer-originated rows must not
/// loop back via delta sync.
pub async fn fetch_delta(
    db: &DatabaseConnection,
    entity_type: &str,
    since: Option<i64>,
    limit: i64,
) -> Result<DeltaWindow, DbErr> {
    let limit = limit.max(1);

    // Cursor-too-old check (D4). Compares against the global oldest
    // retained row (any entity_type / source) because retention is a
    // table-wide policy: any pruning may have removed rows the client
    // hadn't yet acknowledged. Per-entity filtering here would let a
    // fresh client with `since=0` falsely get reset_required just because
    // the first book row sits at id=10 behind nine non-book rows.
    if let Some(cursor) = since {
        let oldest = operation_log::Entity::find()
            .order_by_asc(operation_log::Column::Id)
            .one(db)
            .await?;

        if let Some(row) = oldest {
            let oldest_id = row.id as i64;
            // A cursor of `oldest_id - 1` is still valid (next row is the
            // oldest one we have). Anything strictly less means rows the
            // client hadn't seen could have been pruned.
            if cursor < oldest_id - 1 {
                return Ok(DeltaWindow {
                    operations: Vec::new(),
                    latest_cursor: cursor,
                    has_more: false,
                    reset_required: true,
                });
            }
        }
        // Empty log: no retention happened, cursor is trivially valid.
    }

    // Fetch up to `limit + 1` rows so we can detect `has_more` without a
    // second round trip.
    let since_i32 = since.unwrap_or(0).clamp(0, i32::MAX as i64) as i32;
    let raw_rows = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq(entity_type))
        .filter(operation_log::Column::Source.eq("local"))
        .filter(operation_log::Column::Id.gt(since_i32))
        .order_by_asc(operation_log::Column::Id)
        .limit((limit + 1) as u64)
        .all(db)
        .await?;

    let has_more = raw_rows.len() as i64 > limit;
    let used: &[operation_log::Model] = if has_more {
        &raw_rows[..limit as usize]
    } else {
        &raw_rows
    };

    if used.is_empty() {
        return Ok(DeltaWindow {
            operations: Vec::new(),
            latest_cursor: since.unwrap_or(0),
            has_more: false,
            reset_required: false,
        });
    }

    let latest_cursor = used.last().map(|r| r.id as i64).unwrap_or(0);

    // Collapse: keep latest op per entity_id within the window. Latest op
    // wins ("INSERT then UPDATE" -> upsert, "INSERT then DELETE" -> delete).
    // For sequences ending on UPDATE/INSERT we resolve the current entity
    // state in the HTTP wrapper, so all non-deletes flatten to "upsert".
    struct Entry {
        last_id: i32,
        is_delete: bool,
    }
    let mut by_entity: HashMap<i32, Entry> = HashMap::new();
    for row in used {
        let is_delete = row.operation.eq_ignore_ascii_case("DELETE");
        let entry = by_entity.entry(row.entity_id).or_insert(Entry {
            last_id: row.id,
            is_delete,
        });
        // Latest op wins.
        entry.last_id = row.id;
        entry.is_delete = is_delete;
    }

    let mut collapsed: Vec<DeltaOperation> = by_entity
        .into_iter()
        .map(|(eid, e)| DeltaOperation {
            id: e.last_id as i64,
            entity_type: entity_type.to_string(),
            entity_id: eid,
            op: if e.is_delete {
                DeltaOp::Delete
            } else {
                DeltaOp::Upsert
            },
        })
        .collect();
    collapsed.sort_by_key(|r| r.id);

    Ok(DeltaWindow {
        operations: collapsed,
        latest_cursor,
        has_more,
        reset_required: false,
    })
}

/// Globally oldest retained `operation_log.id`, used by the HTTP wrapper
/// to populate the `oldest_available_cursor` field of a 410 response.
pub async fn oldest_retained_cursor(db: &DatabaseConnection) -> Result<Option<i64>, DbErr> {
    let row = operation_log::Entity::find()
        .order_by_asc(operation_log::Column::Id)
        .one(db)
        .await?;
    Ok(row.map(|r| r.id as i64))
}

/// Current global max `operation_log.id` (or 0 when empty). Used by the
/// full-catalog endpoint as the `X-Catalog-Cursor` header so a peer that
/// just rebuilt state from a full GET can resume on a delta pull without
/// going through the `?since=0` round trip.
pub async fn oldest_or_latest_cursor(db: &DatabaseConnection) -> Result<i64, DbErr> {
    let row = operation_log::Entity::find()
        .order_by_desc(operation_log::Column::Id)
        .one(db)
        .await?;
    Ok(row.map(|r| r.id as i64).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use sea_orm::Set;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:")
            .await
            .expect("init_db in memory")
    }

    /// Insert an operation_log row directly, bypassing the global prune
    /// counter so tests stay isolated. Returns the inserted id.
    async fn insert_log(
        db: &DatabaseConnection,
        entity_type: &str,
        entity_id: i32,
        operation: &str,
        source: &str,
    ) -> i32 {
        let row = operation_log::ActiveModel {
            entity_type: Set(entity_type.to_owned()),
            entity_id: Set(entity_id),
            operation: Set(operation.to_owned()),
            payload: Set(None),
            source: Set(source.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let res = operation_log::Entity::insert(row).exec(db).await.unwrap();
        res.last_insert_id
    }

    #[tokio::test]
    async fn empty_log_returns_empty_window() {
        let db = setup().await;
        let w = fetch_delta(&db, "book", None, 100).await.unwrap();
        assert!(w.operations.is_empty());
        assert!(!w.has_more);
        assert!(!w.reset_required);
        assert_eq!(w.latest_cursor, 0);
    }

    #[tokio::test]
    async fn empty_log_with_explicit_zero_cursor() {
        let db = setup().await;
        let w = fetch_delta(&db, "book", Some(0), 100).await.unwrap();
        assert!(w.operations.is_empty());
        assert!(!w.reset_required);
        assert_eq!(w.latest_cursor, 0);
    }

    #[tokio::test]
    async fn rows_under_limit_returns_all_no_more() {
        let db = setup().await;
        let id1 = insert_log(&db, "book", 1, "INSERT", "local").await;
        let _id2 = insert_log(&db, "book", 2, "INSERT", "local").await;
        let id3 = insert_log(&db, "book", 3, "INSERT", "local").await;

        let w = fetch_delta(&db, "book", Some(0), 10).await.unwrap();
        assert_eq!(w.operations.len(), 3);
        assert!(!w.has_more);
        assert_eq!(w.latest_cursor, id3 as i64);
        assert_eq!(w.operations[0].entity_id, 1);
        assert_eq!(w.operations[0].id, id1 as i64);
        assert_eq!(w.operations[1].entity_id, 2);
        assert_eq!(w.operations[2].entity_id, 3);
        assert!(w.operations.iter().all(|o| o.op == DeltaOp::Upsert));
    }

    #[tokio::test]
    async fn over_limit_caps_and_signals_more() {
        let db = setup().await;
        for i in 1..=5 {
            insert_log(&db, "book", i, "INSERT", "local").await;
        }
        let w = fetch_delta(&db, "book", Some(0), 3).await.unwrap();
        assert_eq!(w.operations.len(), 3);
        assert!(w.has_more);
        // Cursor must be the id of the last row in the window, not the
        // global max — the client re-queries with this cursor.
        let last_returned_entity = w.operations.last().unwrap().entity_id;
        assert_eq!(last_returned_entity, 3);
    }

    #[tokio::test]
    async fn collapse_insert_then_update_yields_one_upsert() {
        let db = setup().await;
        insert_log(&db, "book", 1, "INSERT", "local").await;
        let update_id = insert_log(&db, "book", 1, "UPDATE", "local").await;

        let w = fetch_delta(&db, "book", Some(0), 100).await.unwrap();
        assert_eq!(w.operations.len(), 1);
        assert_eq!(w.operations[0].entity_id, 1);
        assert_eq!(w.operations[0].op, DeltaOp::Upsert);
        // Collapsed row's id is the latest underlying id.
        assert_eq!(w.operations[0].id, update_id as i64);
        assert_eq!(w.latest_cursor, update_id as i64);
    }

    #[tokio::test]
    async fn collapse_insert_update_delete_yields_one_delete() {
        let db = setup().await;
        insert_log(&db, "book", 1, "INSERT", "local").await;
        insert_log(&db, "book", 1, "UPDATE", "local").await;
        let delete_id = insert_log(&db, "book", 1, "DELETE", "local").await;

        let w = fetch_delta(&db, "book", Some(0), 100).await.unwrap();
        assert_eq!(w.operations.len(), 1);
        assert_eq!(w.operations[0].op, DeltaOp::Delete);
        assert_eq!(w.operations[0].id, delete_id as i64);
    }

    #[tokio::test]
    async fn remote_source_rows_are_filtered_out() {
        let db = setup().await;
        insert_log(&db, "book", 1, "INSERT", "device:42").await;
        insert_log(&db, "book", 2, "INSERT", "device:99").await;
        let local_id = insert_log(&db, "book", 3, "INSERT", "local").await;

        let w = fetch_delta(&db, "book", Some(0), 100).await.unwrap();
        assert_eq!(w.operations.len(), 1);
        assert_eq!(w.operations[0].entity_id, 3);
        assert_eq!(w.latest_cursor, local_id as i64);
    }

    #[tokio::test]
    async fn cursor_older_than_oldest_returns_reset_required() {
        let db = setup().await;
        // Simulate a pruned log: oldest local id is way past 0.
        // We can't realistically backfill ids in SQLite without manual
        // INSERTs that set the id, so we instead insert and then prune
        // the early rows.
        let _drop = insert_log(&db, "book", 1, "INSERT", "local").await;
        let _drop2 = insert_log(&db, "book", 2, "INSERT", "local").await;
        let keep = insert_log(&db, "book", 3, "INSERT", "local").await;
        // Manually delete the early rows to simulate retention.
        operation_log::Entity::delete_many()
            .filter(operation_log::Column::Id.lt(keep))
            .exec(&db)
            .await
            .unwrap();

        let w = fetch_delta(&db, "book", Some(0), 100).await.unwrap();
        assert!(
            w.reset_required,
            "cursor 0 must trigger reset when oldest id > 1",
        );
        assert!(w.operations.is_empty());
    }

    #[tokio::test]
    async fn cursor_at_oldest_minus_one_is_valid() {
        let db = setup().await;
        let id1 = insert_log(&db, "book", 1, "INSERT", "local").await;
        // Cursor exactly equal to oldest_id - 1 is valid: next fetch
        // returns the row at oldest_id.
        let w = fetch_delta(&db, "book", Some((id1 - 1) as i64), 100)
            .await
            .unwrap();
        assert!(!w.reset_required);
        assert_eq!(w.operations.len(), 1);
    }

    #[tokio::test]
    async fn other_entity_types_do_not_leak() {
        let db = setup().await;
        insert_log(&db, "contact", 1, "INSERT", "local").await;
        insert_log(&db, "loan", 2, "INSERT", "local").await;
        let book_id = insert_log(&db, "book", 3, "INSERT", "local").await;

        let w = fetch_delta(&db, "book", Some(0), 100).await.unwrap();
        assert_eq!(w.operations.len(), 1);
        assert_eq!(w.operations[0].entity_id, 3);
        assert_eq!(w.latest_cursor, book_id as i64);
    }
}
