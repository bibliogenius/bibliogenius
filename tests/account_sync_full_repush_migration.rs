//! Regression tests for migration 092 (`migrate_account_sync_full_repush`, ADR-056):
//! the one-shot reset of the account-sync push watermark that makes every device
//! republish its entities once, as complete changesets, so the partial blobs left
//! in the hub lane store by the pre-ADR-056 engine are overwritten.
//!
//! The gate matters as much as the reset. Without it the watermark would be
//! cleared on every boot and the whole library re-pushed each time; and a row
//! created after the migration belongs to a device already running the fixed
//! engine, which must not pay a second full push after enrolling.

use rust_lib_app::db;
use rust_lib_app::services::account_sync_engine::{DbSyncStateStore, SyncStateStore};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, Statement};

fn backend(db: &DatabaseConnection) -> sea_orm::DatabaseBackend {
    db.get_database_backend()
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("exec `{sql}` failed: {e}"));
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"))
        .unwrap_or_else(|| panic!("query `{sql}` returned no row"));
    row.try_get::<i64>("", "v").expect("decode v as i64")
}

async fn push_version(db: &DatabaseConnection, account: &str) -> i64 {
    scalar_i64(
        db,
        &format!("SELECT push_version AS v FROM account_sync_state WHERE account_id = '{account}'"),
    )
    .await
}

async fn repush_flag(db: &DatabaseConnection, account: &str) -> i64 {
    scalar_i64(
        db,
        &format!(
            "SELECT full_repush_done AS v FROM account_sync_state WHERE account_id = '{account}'"
        ),
    )
    .await
}

async fn single_conn() -> DatabaseConnection {
    let mut opts = ConnectOptions::new("sqlite::memory:".to_owned());
    opts.max_connections(1).min_connections(1);
    Database::connect(opts).await.expect("connect")
}

/// A device that synced under the old engine: its watermark is cleared exactly
/// once, and a watermark it advances afterwards survives every later boot.
#[tokio::test]
async fn migration_092_clears_a_legacy_watermark_once_and_only_once() {
    let db = single_conn().await;
    db::run_migrations(&db).await.expect("run_migrations");

    // Reproduce a pre-092 install: no flag column, and a watermark left behind by
    // the old engine.
    exec(
        &db,
        "ALTER TABLE account_sync_state DROP COLUMN full_repush_done",
    )
    .await;
    exec(
        &db,
        "INSERT INTO account_sync_state (account_id, pull_cursor, push_version) \
         VALUES ('acct-legacy', 5701, 1853)",
    )
    .await;

    db::run_migrations(&db).await.expect("re-run migrations");
    assert_eq!(
        push_version(&db, "acct-legacy").await,
        0,
        "the legacy watermark is cleared so the next sync re-sends every entity"
    );
    assert_eq!(repush_flag(&db, "acct-legacy").await, 1);
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT pull_cursor AS v FROM account_sync_state WHERE account_id = 'acct-legacy'"
        )
        .await,
        5701,
        "only the push side is reset: re-reading every lane is not needed"
    );

    // The device then syncs and advances its watermark. Later boots must leave it
    // alone, or every launch would re-push the whole library.
    DbSyncStateStore::new(db.clone())
        .set_push_version("acct-legacy", 2000)
        .await
        .expect("set_push_version");
    db::run_migrations(&db).await.expect("third run");
    assert_eq!(
        push_version(&db, "acct-legacy").await,
        2000,
        "the reset is one-shot: a boot after the repair must not clear the watermark"
    );
}

/// A device enrolling after the fix already pushes complete changesets, so it has
/// nothing to repair and must not pay a second full push on its next boot.
#[tokio::test]
async fn migration_092_does_not_re_push_a_device_that_enrolled_after_the_fix() {
    let db = single_conn().await;
    db::run_migrations(&db).await.expect("run_migrations");

    // Enrolment, through the real state store rather than a hand-written INSERT:
    // the flag is set by the same code path production uses.
    let store = DbSyncStateStore::new(db.clone());
    store
        .set_push_version("acct-fresh", 420)
        .await
        .expect("set_push_version");
    assert_eq!(
        repush_flag(&db, "acct-fresh").await,
        1,
        "a row born under the fixed engine needs no repair"
    );

    db::run_migrations(&db).await.expect("re-run migrations");
    assert_eq!(
        push_version(&db, "acct-fresh").await,
        420,
        "enrolling must not cost a redundant full re-push on the next boot"
    );
}

/// The flag must not be clobbered by ordinary sync-state writes: a device that has
/// not yet been repaired keeps its pending reset even after syncing.
#[tokio::test]
async fn a_pending_repair_survives_ordinary_sync_state_writes() {
    let db = single_conn().await;
    db::run_migrations(&db).await.expect("run_migrations");
    exec(
        &db,
        "ALTER TABLE account_sync_state DROP COLUMN full_repush_done",
    )
    .await;
    exec(
        &db,
        "INSERT INTO account_sync_state (account_id, push_version) VALUES ('acct-pending', 900)",
    )
    .await;
    // The column comes back with its repair-pending default without the reset
    // having run yet.
    exec(
        &db,
        "ALTER TABLE account_sync_state ADD COLUMN full_repush_done INTEGER NOT NULL DEFAULT 0",
    )
    .await;

    DbSyncStateStore::new(db.clone())
        .set_pull_cursor("acct-pending", 77)
        .await
        .expect("set_pull_cursor");
    assert_eq!(
        repush_flag(&db, "acct-pending").await,
        0,
        "an ON CONFLICT update must leave the pending repair flag alone"
    );

    db::run_migrations(&db).await.expect("re-run migrations");
    assert_eq!(push_version(&db, "acct-pending").await, 0);
}
