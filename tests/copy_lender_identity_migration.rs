//! Regression tests for migration 089 (`migrate_copy_lender_identity`, ADR-049):
//! the two lender-identity columns on `copies`, added safely on both a plain
//! table and a live cr-sqlite CRR, with the stable identity backfilled from the
//! local peer row.
//!
//! The CRR test (feature `crsqlite-static`) is the permanent replacement for the
//! throwaway alter spike: it drives the REAL `run_migrations` entrypoint over a
//! `copies` table already promoted to a CRR — exactly what an enrolled device
//! does on every boot — and proves the ALTER neither corrupts the clocks nor
//! loses rows, and that edits to the new columns still replicate.

use rust_lib_app::db;
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

async fn opt_string(db: &DatabaseConnection, sql: &str) -> Option<String> {
    let row = db
        .query_one(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"))
        .unwrap_or_else(|| panic!("query `{sql}` returned no row"));
    row.try_get::<Option<String>>("", "v").expect("decode v")
}

async fn column_exists(db: &DatabaseConnection, table: &str, col: &str) -> bool {
    scalar_i64(
        db,
        &format!("SELECT COUNT(*) AS v FROM pragma_table_info('{table}') WHERE name = '{col}'"),
    )
    .await
        > 0
}

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    scalar_i64(db, &format!("SELECT COUNT(*) AS v FROM {table}")).await
}

async fn single_conn(url: &str) -> DatabaseConnection {
    // One connection, like the account-sync pool: begin_alter .. ALTER .. commit_alter
    // must land on the same connection.
    let mut opts = ConnectOptions::new(url.to_owned());
    opts.max_connections(1).min_connections(1);
    Database::connect(opts).await.expect("connect")
}

const SEED_PEER: &str = "INSERT INTO peers (id, name, url, library_uuid) \
    VALUES (5, 'Bob', 'http://bob.local', 'lib-5')";
// A borrowed copy pointing at peer 5.
const SEED_COPY_KNOWN: &str = "INSERT INTO copies \
    (uuid, book_id, library_id, status, is_temporary, lender_peer_id, created_at, updated_at) \
    VALUES ('c-1','b-1',1,'borrowed',0,5,'2026-07-13T00:00:00Z','2026-07-13T00:00:00Z')";
// A borrowed copy whose lender peer is unknown locally (no such peer row).
const SEED_COPY_UNKNOWN: &str = "INSERT INTO copies \
    (uuid, book_id, library_id, status, is_temporary, lender_peer_id, created_at, updated_at) \
    VALUES ('c-2','b-2',1,'borrowed',0,999,'2026-07-13T00:00:00Z','2026-07-13T00:00:00Z')";

/// Plain path (no cr-sqlite): the columns are added, backfilled from the peer row
/// where one resolves, and the migration is a no-op on re-run.
#[tokio::test]
async fn migration_089_adds_columns_backfills_and_is_idempotent() {
    let db = single_conn("sqlite::memory:").await;
    db::run_migrations(&db).await.expect("run_migrations");
    assert!(column_exists(&db, "copies", "lender_library_uuid").await);
    assert!(column_exists(&db, "copies", "lender_request_id").await);

    // Reproduce a pre-089 install that already holds borrowed copies: drop the
    // columns, seed a peer plus two borrowed copies (one resolvable, one not),
    // then re-run the migration entrypoint so 089 runs against existing data.
    exec(&db, "ALTER TABLE copies DROP COLUMN lender_library_uuid").await;
    exec(&db, "ALTER TABLE copies DROP COLUMN lender_request_id").await;
    exec(&db, SEED_PEER).await;
    exec(&db, SEED_COPY_KNOWN).await;
    exec(&db, SEED_COPY_UNKNOWN).await;

    db::run_migrations(&db).await.expect("re-run migrations");

    assert!(column_exists(&db, "copies", "lender_library_uuid").await);
    assert!(column_exists(&db, "copies", "lender_request_id").await);
    // Backfill: the resolvable copy gets the peer's stable identity...
    assert_eq!(
        opt_string(
            &db,
            "SELECT lender_library_uuid AS v FROM copies WHERE uuid = 'c-1'"
        )
        .await,
        Some("lib-5".to_owned()),
        "the copy borrowed from a known peer must inherit its library_uuid"
    );
    // ...and the unknown-peer copy stays NULL rather than erroring.
    assert_eq!(
        opt_string(
            &db,
            "SELECT lender_library_uuid AS v FROM copies WHERE uuid = 'c-2'"
        )
        .await,
        None,
        "a copy whose lender peer is unknown locally must stay NULL"
    );
    // lender_request_id has no local source: NULL for both, filled at borrow time.
    assert_eq!(
        opt_string(
            &db,
            "SELECT lender_request_id AS v FROM copies WHERE uuid = 'c-1'"
        )
        .await,
        None
    );

    // Third run: pure no-op, nothing lost.
    db::run_migrations(&db).await.expect("third run is a no-op");
    assert_eq!(count(&db, "copies").await, 2);
}

/// Live-CRR path (enrolled device): migration 089 runs against a `copies` table
/// already promoted to a cr-sqlite CRR, exactly as it does on every boot. It must
/// add the columns via `begin_alter`/`commit_alter`, lose no rows, keep the clock
/// machinery, backfill the identity, and keep capturing edits to the new columns.
#[cfg(feature = "crsqlite-static")]
#[tokio::test]
async fn migration_089_evolves_a_live_copies_crr_without_corruption() {
    use rust_lib_app::infrastructure::{crsqlite_crr, crsqlite_static};

    crsqlite_static::register();
    let path = std::env::temp_dir().join("bg_mig089_crr.db");
    for ext in ["db", "db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());

    let db = single_conn(&url).await;
    db::run_migrations(&db).await.expect("run_migrations");
    crsqlite_crr::setup_crrs(&db).await.expect("setup_crrs");

    // Seed a peer and a borrowed copy while the CRR carries the 089 columns.
    exec(&db, SEED_PEER).await;
    exec(&db, SEED_COPY_KNOWN).await;

    // Bring the table back to the pre-089 shape the enrolled device is in, through
    // the same alter protocol so it stays a valid CRR.
    exec(&db, "SELECT crsql_begin_alter('copies')").await;
    exec(&db, "ALTER TABLE copies DROP COLUMN lender_library_uuid").await;
    exec(&db, "ALTER TABLE copies DROP COLUMN lender_request_id").await;
    exec(&db, "SELECT crsql_commit_alter('copies')").await;
    assert!(!column_exists(&db, "copies", "lender_library_uuid").await);

    // Run the REAL migration entrypoint on the live CRR.
    db::run_migrations(&db).await.expect("089 on a live CRR");

    // Columns re-added, row preserved, CRR intact.
    assert!(column_exists(&db, "copies", "lender_library_uuid").await);
    assert!(column_exists(&db, "copies", "lender_request_id").await);
    assert_eq!(
        count(&db, "copies").await,
        1,
        "no row lost across the ALTER"
    );
    assert!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM sqlite_master \
             WHERE type = 'table' AND name = 'copies__crsql_clock'"
        )
        .await
            > 0,
        "the clock companion table must survive"
    );
    // Backfill reached through the CRR path.
    assert_eq!(
        opt_string(
            &db,
            "SELECT lender_library_uuid AS v FROM copies WHERE uuid = 'c-1'"
        )
        .await,
        Some("lib-5".to_owned())
    );

    // An edit to the new column must still be captured for replication.
    exec(
        &db,
        "UPDATE copies SET lender_request_id = 'req-9', \
         updated_at = '2026-07-13T02:00:00Z' WHERE uuid = 'c-1'",
    )
    .await;
    let captured = scalar_i64(
        &db,
        "SELECT COUNT(*) AS v FROM crsql_changes \
         WHERE \"table\" = 'copies' AND \"cid\" = 'lender_request_id'",
    )
    .await;
    assert!(captured > 0, "the new column must feed crsql_changes");

    crsqlite_crr::finalize(&db).await.expect("finalize");
    db.close().await.expect("close");
}
