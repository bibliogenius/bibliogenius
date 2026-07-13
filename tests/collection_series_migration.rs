//! Regression tests for migration 090 (`migrate_collection_book_volume_number`):
//! the nullable `volume_number` column on `collection_books` (reading order of a
//! series-typed collection), added safely on both a plain table and a live
//! cr-sqlite CRR.
//!
//! `collection_books` is a replicated CRR (`crsqlite_crr::CRR_TABLES`), so the
//! CRR test (feature `crsqlite-static`) drives the REAL `run_migrations`
//! entrypoint over a table already promoted to a CRR -- exactly what an enrolled
//! device does on every boot -- and proves the ALTER neither corrupts the clocks
//! nor loses rows, and that edits to the new column still replicate.

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

const SEED_COLLECTION: &str = "INSERT INTO collections (id, name, description, source, created_at, updated_at) \
    VALUES ('col-1', 'Cycle', NULL, 'series', '2026-07-13T00:00:00Z', '2026-07-13T00:00:00Z')";
const SEED_BOOK: &str = "INSERT INTO books (uuid, title, reading_status, owned, created_at, updated_at) \
    VALUES ('b-1', 'Tome I', 'to_read', 1, '2026-07-13T00:00:00Z', '2026-07-13T00:00:00Z')";
const SEED_LINK: &str = "INSERT INTO collection_books (collection_id, book_id, added_at) \
    VALUES ('col-1', 'b-1', '2026-07-13T00:00:00Z')";

/// Plain path (no cr-sqlite): the column is added and the migration is a no-op on
/// re-run, preserving pre-existing links (which stay NULL / unnumbered).
#[tokio::test]
async fn migration_090_adds_column_and_is_idempotent() {
    let db = single_conn("sqlite::memory:").await;
    db::run_migrations(&db).await.expect("run_migrations");
    assert!(column_exists(&db, "collection_books", "volume_number").await);

    // Reproduce a pre-090 install that already holds a series link: drop the
    // column, seed a collection + book + link, then re-run the entrypoint so 090
    // runs against existing data.
    exec(
        &db,
        "ALTER TABLE collection_books DROP COLUMN volume_number",
    )
    .await;
    exec(&db, SEED_COLLECTION).await;
    exec(&db, SEED_BOOK).await;
    exec(&db, SEED_LINK).await;

    db::run_migrations(&db).await.expect("re-run migrations");

    assert!(column_exists(&db, "collection_books", "volume_number").await);
    assert_eq!(count(&db, "collection_books").await, 1, "link preserved");
    assert_eq!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM collection_books WHERE volume_number IS NULL",
        )
        .await,
        1,
        "the pre-existing link is unnumbered (NULL) after the additive migration",
    );

    // Third run: pure no-op, nothing lost.
    db::run_migrations(&db).await.expect("third run is a no-op");
    assert_eq!(count(&db, "collection_books").await, 1);
}

/// Live-CRR path (enrolled device): migration 090 runs against a `collection_books`
/// table already promoted to a cr-sqlite CRR, exactly as it does on every boot. It
/// must add the column via `begin_alter`/`commit_alter`, lose no rows, keep the
/// clock machinery, and keep capturing edits to the new column.
#[cfg(feature = "crsqlite-static")]
#[tokio::test]
async fn migration_090_evolves_a_live_collection_books_crr_without_corruption() {
    use rust_lib_app::infrastructure::{crsqlite_crr, crsqlite_static};

    crsqlite_static::register();
    let path = std::env::temp_dir().join("bg_mig090_crr.db");
    for ext in ["db", "db-wal", "db-shm"] {
        let _ = std::fs::remove_file(path.with_extension(ext));
    }
    let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());

    let db = single_conn(&url).await;
    db::run_migrations(&db).await.expect("run_migrations");
    crsqlite_crr::setup_crrs(&db).await.expect("setup_crrs");

    // Seed a series collection, a book, and a link while the CRR carries the 090
    // column.
    exec(&db, SEED_COLLECTION).await;
    exec(&db, SEED_BOOK).await;
    exec(&db, SEED_LINK).await;

    // Bring the table back to the pre-090 shape the enrolled device is in, through
    // the same alter protocol so it stays a valid CRR.
    exec(&db, "SELECT crsql_begin_alter('collection_books')").await;
    exec(
        &db,
        "ALTER TABLE collection_books DROP COLUMN volume_number",
    )
    .await;
    exec(&db, "SELECT crsql_commit_alter('collection_books')").await;
    assert!(!column_exists(&db, "collection_books", "volume_number").await);

    // Run the REAL migration entrypoint on the live CRR.
    db::run_migrations(&db).await.expect("090 on a live CRR");

    // Column re-added, row preserved, CRR intact.
    assert!(column_exists(&db, "collection_books", "volume_number").await);
    assert_eq!(
        count(&db, "collection_books").await,
        1,
        "no row lost across the ALTER"
    );
    assert!(
        scalar_i64(
            &db,
            "SELECT COUNT(*) AS v FROM sqlite_master \
             WHERE type = 'table' AND name = 'collection_books__crsql_clock'"
        )
        .await
            > 0,
        "the clock companion table must survive"
    );

    // An edit to the new column must still be captured for replication.
    exec(
        &db,
        "UPDATE collection_books SET volume_number = 3 WHERE book_id = 'b-1'",
    )
    .await;
    let captured = scalar_i64(
        &db,
        "SELECT COUNT(*) AS v FROM crsql_changes \
         WHERE \"table\" = 'collection_books' AND \"cid\" = 'volume_number'",
    )
    .await;
    assert!(captured > 0, "the new column must feed crsql_changes");

    crsqlite_crr::finalize(&db).await.expect("finalize");
    db.close().await.expect("close");
}
