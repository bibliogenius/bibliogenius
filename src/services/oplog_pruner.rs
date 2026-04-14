//! Hybrid retention pruner for `operation_log` (ADR-028 D5).
//!
//! Keeps the most recent 90 days OR the most recent 10 000 rows, whichever
//! retains *more* data. Configurable via `OPLOG_RETENTION_DAYS` and
//! `OPLOG_RETENTION_MIN_ROWS` env vars. `pending` and `pending_review` rows
//! are never eligible — they would lose unreplayed work.
//!
//! Designed to coexist with the legacy inline prune in `crate::sync`: the
//! inline cap was raised to act as a runaway safety net only, while this
//! module enforces the actual policy on a daily schedule plus once at boot.

use crate::models::operation_log;
use sea_orm::*;

/// Default values mirror ADR-028 D5.
const DEFAULT_RETENTION_DAYS: i64 = 90;
const DEFAULT_MIN_ROWS: i64 = 10_000;

#[derive(Debug, Clone, Copy)]
pub struct PrunePolicy {
    pub retention_days: i64,
    pub min_rows: i64,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            min_rows: DEFAULT_MIN_ROWS,
        }
    }
}

impl PrunePolicy {
    pub fn from_env() -> Self {
        let retention_days = std::env::var("OPLOG_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RETENTION_DAYS);
        let min_rows = std::env::var("OPLOG_RETENTION_MIN_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MIN_ROWS);
        Self {
            retention_days,
            min_rows,
        }
    }
}

/// Run one prune cycle. Returns the number of rows actually deleted.
pub async fn prune_once(db: &DatabaseConnection, policy: &PrunePolicy) -> Result<u64, DbErr> {
    // 1. Resolve the global max id. Empty table = nothing to do.
    let Some(last_row) = operation_log::Entity::find()
        .order_by_desc(operation_log::Column::Id)
        .one(db)
        .await?
    else {
        return Ok(0);
    };
    let last_id = last_row.id as i64;

    // 2. Floor by row count: id of the (min_rows)-th most recent row.
    //    Anything strictly lower is eligible to drop.
    let by_count = (last_id - policy.min_rows + 1).max(1);

    // 3. Floor by age: oldest id within the retention window.
    let cutoff_date =
        (chrono::Utc::now() - chrono::Duration::days(policy.retention_days)).to_rfc3339();
    let oldest_in_window = operation_log::Entity::find()
        .filter(operation_log::Column::CreatedAt.gte(cutoff_date))
        .order_by_asc(operation_log::Column::Id)
        .one(db)
        .await?;
    // No rows in the retention window: the count floor decides on its own.
    let by_age = oldest_in_window.map(|r| r.id as i64).unwrap_or(last_id + 1);

    // 4. Final floor = the more lenient of the two (keeps more rows).
    //    The ADR's natural-language description ("whichever is larger")
    //    is the source of truth here; the formula in the ADR text used
    //    `max(...)` which gives the opposite (more aggressive) cut. The
    //    discrepancy is documented in the ADR Implementation Notes.
    let floor = by_count.min(by_age);

    if floor <= 1 {
        return Ok(0);
    }

    // 5. Drop applied/skipped rows below the floor. Pending + pending_review
    //    + failed are preserved so we never lose unreplayed or audit data.
    let result = operation_log::Entity::delete_many()
        .filter(operation_log::Column::Id.lt(floor as i32))
        .filter(operation_log::Column::Status.is_in(["applied", "skipped"]))
        .exec(db)
        .await?;

    if result.rows_affected > 0 {
        tracing::info!(
            "oplog_pruner: pruned {} row(s) below id {} (by_count={}, by_age={})",
            result.rows_affected,
            floor,
            by_count,
            by_age
        );
    }
    Ok(result.rows_affected)
}

/// Spawn the background task: one prune at startup, then daily.
///
/// The interval timer fires immediately on first tick, so we await it
/// once before entering the loop to space out the daily run from the
/// startup run rather than firing them back-to-back.
pub fn spawn(db: DatabaseConnection) {
    let policy = PrunePolicy::from_env();
    tokio::spawn(async move {
        if let Err(e) = prune_once(&db, &policy).await {
            tracing::warn!("oplog_pruner startup: {e}");
        }
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(86_400));
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            if let Err(e) = prune_once(&db, &policy).await {
                tracing::warn!("oplog_pruner daily: {e}");
            }
        }
    });
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

    /// Insert a row at a known created_at offset from now (negative days = past).
    async fn insert_at(db: &DatabaseConnection, days_offset: i64, status: &str) -> i32 {
        let when = (chrono::Utc::now() - chrono::Duration::days(-days_offset)).to_rfc3339();
        // days_offset < 0 yields a past date; passing positive yields future.
        let row = operation_log::ActiveModel {
            entity_type: Set("book".to_owned()),
            entity_id: Set(1),
            operation: Set("INSERT".to_owned()),
            payload: Set(None),
            source: Set("local".to_owned()),
            status: Set(status.to_owned()),
            created_at: Set(when),
            ..Default::default()
        };
        let res = operation_log::Entity::insert(row).exec(db).await.unwrap();
        res.last_insert_id
    }

    #[tokio::test]
    async fn no_prune_when_under_both_floors() {
        let db = setup().await;
        for _ in 0..50 {
            insert_at(&db, -1, "applied").await;
        }
        let policy = PrunePolicy {
            retention_days: 90,
            min_rows: 10_000,
        };
        let pruned = prune_once(&db, &policy).await.unwrap();
        assert_eq!(pruned, 0);
        let count = operation_log::Entity::find().count(&db).await.unwrap();
        assert_eq!(count, 50);
    }

    #[tokio::test]
    async fn prunes_down_to_min_rows_when_over_count_floor() {
        let db = setup().await;
        // 100 ancient rows (120 days ago) — outside the retention window.
        // min_rows = 10 → only the 10 most recent ids are retained.
        for _ in 0..100 {
            insert_at(&db, -120, "applied").await;
        }
        let policy = PrunePolicy {
            retention_days: 90,
            min_rows: 10,
        };
        let pruned = prune_once(&db, &policy).await.unwrap();
        assert_eq!(pruned, 90);
        let kept = operation_log::Entity::find().count(&db).await.unwrap();
        assert_eq!(kept, 10);
    }

    #[tokio::test]
    async fn keeps_rows_in_retention_window_even_when_above_min_rows() {
        let db = setup().await;
        // 30 old rows (120 days ago) + 50 recent rows (today).
        for _ in 0..30 {
            insert_at(&db, -120, "applied").await;
        }
        for _ in 0..50 {
            insert_at(&db, -1, "applied").await;
        }
        // min_rows = 10: by_count would prune everything older than the
        // 10 most recent. by_age (90d window) keeps the 50 recent rows.
        // MIN floor wins → keep all 50 recent.
        let policy = PrunePolicy {
            retention_days: 90,
            min_rows: 10,
        };
        let pruned = prune_once(&db, &policy).await.unwrap();
        assert_eq!(pruned, 30);
        let kept = operation_log::Entity::find().count(&db).await.unwrap();
        assert_eq!(kept, 50);
    }

    #[tokio::test]
    async fn count_floor_dominates_when_recent_window_is_smaller() {
        let db = setup().await;
        // 5 old rows (120 days ago) + 5 recent rows. min_rows = 8.
        // by_age = id of first recent row (6) → would keep 5 rows.
        // by_count = last_id - 7 = 10 - 7 = 3 → would keep id >= 3 (8 rows).
        // MIN floor = 3 → keep 8 rows (3 of the old, 5 recent).
        for _ in 0..5 {
            insert_at(&db, -120, "applied").await;
        }
        for _ in 0..5 {
            insert_at(&db, -1, "applied").await;
        }
        let policy = PrunePolicy {
            retention_days: 90,
            min_rows: 8,
        };
        let pruned = prune_once(&db, &policy).await.unwrap();
        assert_eq!(pruned, 2);
        let kept = operation_log::Entity::find().count(&db).await.unwrap();
        assert_eq!(kept, 8);
    }

    #[tokio::test]
    async fn pending_rows_are_never_pruned() {
        let db = setup().await;
        // 50 ancient applied + 5 ancient pending. min_rows = 1.
        for _ in 0..50 {
            insert_at(&db, -120, "applied").await;
        }
        for _ in 0..5 {
            insert_at(&db, -120, "pending").await;
        }
        let policy = PrunePolicy {
            retention_days: 90,
            min_rows: 1,
        };
        prune_once(&db, &policy).await.unwrap();
        let pending_left = operation_log::Entity::find()
            .filter(operation_log::Column::Status.eq("pending"))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(pending_left, 5, "pending rows must survive prune");
    }

    #[tokio::test]
    async fn pending_review_and_failed_also_preserved() {
        let db = setup().await;
        for _ in 0..20 {
            insert_at(&db, -120, "applied").await;
        }
        let pr_id = insert_at(&db, -120, "pending_review").await;
        let f_id = insert_at(&db, -120, "failed").await;
        let policy = PrunePolicy {
            retention_days: 90,
            min_rows: 1,
        };
        prune_once(&db, &policy).await.unwrap();
        assert!(
            operation_log::Entity::find_by_id(pr_id)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "pending_review must survive"
        );
        assert!(
            operation_log::Entity::find_by_id(f_id)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "failed must survive"
        );
    }

    #[tokio::test]
    async fn empty_table_is_a_noop() {
        let db = setup().await;
        let pruned = prune_once(&db, &PrunePolicy::default()).await.unwrap();
        assert_eq!(pruned, 0);
    }

    #[test]
    fn env_var_override() {
        // Save and restore so we don't pollute other tests.
        let prev_days = std::env::var("OPLOG_RETENTION_DAYS").ok();
        let prev_rows = std::env::var("OPLOG_RETENTION_MIN_ROWS").ok();
        // SAFETY: tests share the process env; this case is single-threaded.
        unsafe {
            std::env::set_var("OPLOG_RETENTION_DAYS", "7");
            std::env::set_var("OPLOG_RETENTION_MIN_ROWS", "42");
        }
        let policy = PrunePolicy::from_env();
        assert_eq!(policy.retention_days, 7);
        assert_eq!(policy.min_rows, 42);
        unsafe {
            match prev_days {
                Some(v) => std::env::set_var("OPLOG_RETENTION_DAYS", v),
                None => std::env::remove_var("OPLOG_RETENTION_DAYS"),
            }
            match prev_rows {
                Some(v) => std::env::set_var("OPLOG_RETENTION_MIN_ROWS", v),
                None => std::env::remove_var("OPLOG_RETENTION_MIN_ROWS"),
            }
        }
    }
}
