//! Regression tests for migration 091 (`migrate_collection_book_added_at`):
//! `collection_books.added_at` is `TEXT NOT NULL DEFAULT ''` on a replicated
//! cr-sqlite CRR, so an insert that omits the column, a row synced from a peer
//! holding a legacy value, or one restored from an old backup can carry an
//! empty or non-ISO date. Such a value crashed the Flutter Collections tab on
//! `DateTime.parse`; the reader was hardened, and this migration repairs the
//! stored data at each boot.
//!
//! The migration is a plain `UPDATE` (not DDL), so it needs no alter protocol.
//! The plain path proves the backfill and its idempotence; the CRR path (feature
//! `crsqlite-static`) drives the REAL `run_migrations` entrypoint over a live
//! CRR -- exactly what an enrolled device does on every boot -- and proves the
//! repair loses no rows, keeps the clock machinery, and replicates to peers.

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

async fn scalar_string(db: &DatabaseConnection, sql: &str) -> String {
    let row = db
        .query_one(Statement::from_string(backend(db), sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("query `{sql}` failed: {e}"))
        .unwrap_or_else(|| panic!("query `{sql}` returned no row"));
    row.try_get::<String>("", "v").expect("decode v as String")
}

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    scalar_i64(db, &format!("SELECT COUNT(*) AS v FROM {table}")).await
}

async fn invalid_added_at_count(db: &DatabaseConnection) -> i64 {
    scalar_i64(
        db,
        "SELECT COUNT(*) AS v FROM collection_books \
         WHERE added_at IS NULL OR datetime(added_at) IS NULL",
    )
    .await
}

async fn added_at_of(db: &DatabaseConnection, book_id: &str) -> String {
    scalar_string(
        db,
        &format!("SELECT added_at AS v FROM collection_books WHERE book_id = '{book_id}'"),
    )
    .await
}

async fn single_conn(url: &str) -> DatabaseConnection {
    let mut opts = ConnectOptions::new(url.to_owned());
    opts.max_connections(1).min_connections(1);
    Database::connect(opts).await.expect("connect")
}

const GOOD_DATE: &str = "2026-07-13T00:00:00Z";
const EPOCH: &str = "1970-01-01T00:00:00+00:00";
/// The nanosecond-precision offset form chrono emits. SQLite's `datetime()`
/// must accept it, or the migration would clobber a valid date with the
/// backfill value.
const NANO_DATE: &str = "2026-07-13T10:20:30.123456789+00:00";

// A collection whose own created_at is valid: it is the backfill source for its
// links.
const SEED_COLLECTION: &str = "INSERT INTO collections (id, name, description, source, created_at, updated_at) \
    VALUES ('col-1', 'Cycle', NULL, 'series', '2026-07-13T00:00:00Z', '2026-07-13T00:00:00Z')";
// A collection whose created_at is itself broken (empty): links fall back to the
// epoch sentinel rather than propagating another invalid value.
const SEED_COLLECTION_BADDATE: &str = "INSERT INTO collections (id, name, description, source, created_at, updated_at) \
    VALUES ('col-2', 'Broken', NULL, 'series', '', '2026-07-13T00:00:00Z')";

fn seed_book(book_id: &str) -> String {
    format!(
        "INSERT INTO books (uuid, title, reading_status, owned, created_at, updated_at) \
         VALUES ('{book_id}', 'Tome', 'to_read', 1, '2026-07-13T00:00:00Z', '2026-07-13T00:00:00Z')"
    )
}

fn seed_link(collection_id: &str, book_id: &str, added_at: &str) -> String {
    format!(
        "INSERT INTO collection_books (collection_id, book_id, added_at) \
         VALUES ('{collection_id}', '{book_id}', '{added_at}')"
    )
}

/// Seed two valid links (plain and nanosecond-precision) plus four broken ones
/// (empty, non-ISO, one whose parent collection date is also broken, and one
/// orphaned link with no parent collection row) on an existing schema.
async fn seed_broken_links(db: &DatabaseConnection) {
    exec(db, SEED_COLLECTION).await;
    exec(db, SEED_COLLECTION_BADDATE).await;
    for book_id in [
        "b-good",
        "b-nano",
        "b-empty",
        "b-garbage",
        "b-fallback",
        "b-orphan",
    ] {
        exec(db, &seed_book(book_id)).await;
    }
    exec(db, &seed_link("col-1", "b-good", GOOD_DATE)).await;
    exec(db, &seed_link("col-1", "b-nano", NANO_DATE)).await;
    exec(db, &seed_link("col-1", "b-empty", "")).await;
    exec(db, &seed_link("col-1", "b-garbage", "not-a-date")).await;
    exec(db, &seed_link("col-2", "b-fallback", "")).await;
    // The replicated tables carry no FK (ADR-044), so an orphaned link is a
    // legal state a peer sync or partial restore can produce.
    exec(db, &seed_link("col-gone", "b-orphan", "")).await;
}

/// Plain path (no cr-sqlite): broken `added_at` values are repaired from the
/// parent collection's `created_at` (or the epoch when that is broken too), a
/// valid value is left untouched, no row is lost, and a re-run is a no-op.
#[tokio::test]
async fn migration_091_backfills_invalid_added_at_and_is_idempotent() {
    let db = single_conn("sqlite::memory:").await;
    db::run_migrations(&db).await.expect("run_migrations");

    seed_broken_links(&db).await;
    assert_eq!(
        invalid_added_at_count(&db).await,
        4,
        "four broken links are seeded before the migration"
    );

    db::run_migrations(&db).await.expect("re-run migrations");

    assert_eq!(
        invalid_added_at_count(&db).await,
        0,
        "no invalid added_at survives the backfill"
    );
    assert_eq!(
        added_at_of(&db, "b-good").await,
        GOOD_DATE,
        "a valid value is left untouched"
    );
    assert_eq!(
        added_at_of(&db, "b-nano").await,
        NANO_DATE,
        "a valid nanosecond-precision date is left byte-identical"
    );
    assert_eq!(
        added_at_of(&db, "b-orphan").await,
        EPOCH,
        "an orphaned link (no parent collection row) falls back to the epoch"
    );
    assert_eq!(
        added_at_of(&db, "b-empty").await,
        GOOD_DATE,
        "an empty value is backfilled from the parent collection's created_at"
    );
    assert_eq!(
        added_at_of(&db, "b-garbage").await,
        GOOD_DATE,
        "a non-ISO value is backfilled from the parent collection's created_at"
    );
    assert_eq!(
        added_at_of(&db, "b-fallback").await,
        EPOCH,
        "when the parent collection's date is also broken, fall back to the epoch"
    );
    assert_eq!(count(&db, "collection_books").await, 6, "no link lost");

    // Third run: pure no-op, nothing changes.
    db::run_migrations(&db).await.expect("third run is a no-op");
    assert_eq!(invalid_added_at_count(&db).await, 0);
    assert_eq!(count(&db, "collection_books").await, 6);
}

/// Live-CRR path (enrolled device): migration 091 repairs a broken `added_at` on
/// a `collection_books` table already promoted to a cr-sqlite CRR. The `UPDATE`
/// must flow through the CRR triggers (feeding `crsql_changes` so the repair
/// replicates), lose no rows, and keep the clock machinery intact.
#[cfg(feature = "crsqlite-static")]
#[tokio::test]
async fn migration_091_repairs_added_at_on_a_live_crr_and_replicates() {
    use rust_lib_app::infrastructure::{crsqlite_crr, crsqlite_static};

    crsqlite_static::register();
    let path = std::env::temp_dir().join("bg_mig091_crr.db");
    for ext in ["db", "db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());

    let db = single_conn(&url).await;
    db::run_migrations(&db).await.expect("run_migrations");
    crsqlite_crr::setup_crrs(&db).await.expect("setup_crrs");

    // Seed a broken link while the table is a live CRR, then drain the changes so
    // the assertion below sees only what the migration itself captures.
    exec(&db, SEED_COLLECTION).await;
    exec(&db, &seed_book("b-empty")).await;
    exec(&db, &seed_link("col-1", "b-empty", "")).await;
    assert_eq!(
        invalid_added_at_count(&db).await,
        1,
        "one broken link seeded"
    );

    // Run the REAL migration entrypoint on the live CRR.
    db::run_migrations(&db).await.expect("091 on a live CRR");

    assert_eq!(
        invalid_added_at_count(&db).await,
        0,
        "the broken link is repaired on the CRR"
    );
    assert_eq!(
        added_at_of(&db, "b-empty").await,
        GOOD_DATE,
        "repaired from the parent collection's created_at"
    );
    assert_eq!(count(&db, "collection_books").await, 1, "no row lost");
    assert!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM sqlite_master \
             WHERE type = 'table' AND name = 'collection_books__crsql_clock'"
        )
        .await
            > 0,
        "the clock companion table must survive the UPDATE"
    );
    assert!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM crsql_changes \
             WHERE \"table\" = 'collection_books' AND \"cid\" = 'added_at'"
        )
        .await
            > 0,
        "the repair must feed crsql_changes so it replicates to peers"
    );

    crsqlite_crr::finalize(&db).await.expect("finalize");
    db.close().await.expect("close");
}
