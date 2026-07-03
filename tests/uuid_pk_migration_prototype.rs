//! Validation of the id -> uuid primary-key migration (`migrate_uuid_pk`, now
//! wired into `run_migrations`, ADR-044 Addendum A + B).
//!
//! Because the migration runs as the last step of `run_migrations`, every
//! `init_db` already yields the uuid-PK schema, so these tests assert the
//! resulting shape directly:
//!   - the six replicated entity tables have a `uuid TEXT PRIMARY KEY` and no
//!     FOREIGN KEY clauses (cr-sqlite CRR rule),
//!   - junctions keep a composite PRIMARY KEY,
//!   - local tables (`sales`) keep their integer `id`, and the module table
//!     `book_notes` had its `book_id` ref rewritten to uuid TEXT,
//!   - references to local tables (`copies.library_id`) stay INTEGER,
//!   - the migration is idempotent (a direct re-call is a no-op).
//!
//! The data-preservation property (no row lost, no orphaned reference after the
//! flip) is validated against a COPY of a REAL library via the `WS2_REAL_DB`
//! gate below: row counts are captured BEFORE `run_migrations` (old integer-id
//! schema) and re-checked after the flip. The synthetic fixture can no longer
//! exercise it because `run_migrations` flips the schema before any row with an
//! integer id could be seeded.

use rust_lib_app::db;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sqlx::Row;

/// Every reference that must point at a parent's uuid after the migration, plus the
/// parent's uuid-bearing column (`collections` is already uuid-keyed via `id`).
const ALL_REFS: &[(&str, &str, &str, &str)] = &[
    // (child_table, child_col, parent_table, parent_uuid_col)
    ("copies", "book_id", "books", "uuid"),
    ("loans", "copy_id", "copies", "uuid"),
    ("loans", "contact_id", "contacts", "uuid"),
    ("tags", "parent_id", "tags", "uuid"),
    ("book_authors", "book_id", "books", "uuid"),
    ("book_authors", "author_id", "authors", "uuid"),
    ("book_tags", "book_id", "books", "uuid"),
    ("book_tags", "tag_id", "tags", "uuid"),
    ("collection_books", "book_id", "books", "uuid"),
    ("collection_books", "collection_id", "collections", "id"),
    ("sales", "copy_id", "copies", "uuid"),
    ("sales", "contact_id", "contacts", "uuid"),
    ("book_notes", "book_id", "books", "uuid"),
];

/// Tables whose row count must be preserved across the migration.
const PLAN_TABLES: &[&str] = &[
    "books",
    "authors",
    "tags",
    "contacts",
    "copies",
    "loans",
    "book_authors",
    "book_tags",
    "collection_books",
    "sales",
    "book_notes",
];

async fn setup() -> DatabaseConnection {
    db::init_db("sqlite::memory:").await.expect("init db")
}

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT COUNT(*) AS c FROM \"{table}\""),
        ))
        .await
        .expect("count query")
        .expect("count row");
    row.try_get::<i64>("", "c").expect("count value")
}

async fn count_where(db: &DatabaseConnection, table: &str, cond: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT COUNT(*) AS c FROM \"{table}\" WHERE {cond}"),
        ))
        .await
        .expect("count query")
        .expect("count row");
    row.try_get::<i64>("", "c").expect("count value")
}

async fn capture_counts(db: &DatabaseConnection) -> Vec<(String, i64)> {
    let mut v = Vec::new();
    for table in PLAN_TABLES {
        v.push((table.to_string(), count(db, table).await));
    }
    v
}

async fn assert_counts_preserved(db: &DatabaseConnection, before: &[(String, i64)]) {
    for (table, n) in before {
        assert_eq!(
            count(db, table).await,
            *n,
            "row count changed for {table}: the rebuild lost or duplicated rows"
        );
    }
}

async fn count_orphans(
    db: &DatabaseConnection,
    child: &str,
    col: &str,
    parent: &str,
    parent_col: &str,
) -> i64 {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!(
                "SELECT COUNT(*) AS c FROM \"{child}\" \
                 WHERE \"{col}\" IS NOT NULL \
                 AND \"{col}\" NOT IN (SELECT \"{parent_col}\" FROM \"{parent}\")"
            ),
        ))
        .await
        .expect("orphan query")
        .expect("orphan row");
    row.try_get::<i64>("", "c").expect("orphan count")
}

async fn assert_no_orphans(db: &DatabaseConnection) {
    for (child, col, parent, parent_col) in ALL_REFS {
        let orphans = count_orphans(db, child, col, parent, parent_col).await;
        assert_eq!(
            orphans, 0,
            "{child}.{col} has {orphans} value(s) not present in {parent}.{parent_col} after the rewrite"
        );
    }
}

/// `CREATE TABLE` SQL as stored by SQLite, for structural assertions.
async fn table_sql(db: &DatabaseConnection, table: &str) -> String {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT sql FROM sqlite_master WHERE type='table' AND name='{table}'"),
        ))
        .await
        .expect("schema query")
        .expect("schema row");
    row.try_get::<String>("", "sql").expect("schema sql")
}

async fn column_type(db: &DatabaseConnection, table: &str, column: &str) -> String {
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.unwrap();
    let rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
        .fetch_all(&mut *conn)
        .await
        .expect("table_info");
    rows.iter()
        .find(|r| r.get::<String, _>("name") == column)
        .map(|r| r.get::<String, _>("type"))
        .unwrap_or_else(|| panic!("{table}.{column} not present"))
}

/// `(has_column, is_primary_key)` for `column` of `table`, via PRAGMA table_info.
async fn column_pk(db: &DatabaseConnection, table: &str, column: &str) -> (bool, bool) {
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.unwrap();
    let rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
        .fetch_all(&mut *conn)
        .await
        .expect("table_info");
    match rows.iter().find(|r| r.get::<String, _>("name") == column) {
        Some(r) => (true, r.get::<i64, _>("pk") != 0),
        None => (false, false),
    }
}

#[tokio::test]
async fn replicated_tables_have_uuid_pk_and_no_foreign_keys() {
    let db = setup().await;

    for table in ["books", "authors", "tags", "contacts", "copies", "loans"] {
        // `uuid` is the PRIMARY KEY (it carries a `DEFAULT (uuid_v7)` so inserts
        // that bypass the Rust `before_save` hook still get one).
        let (uuid_present, uuid_is_pk) = column_pk(&db, table, "uuid").await;
        assert!(
            uuid_present && uuid_is_pk,
            "{table}.uuid must be the PRIMARY KEY"
        );
        // The device-local integer `id` column is gone.
        let (id_present, _) = column_pk(&db, table, "id").await;
        assert!(
            !id_present,
            "{table} must no longer carry the integer id column"
        );
        // No FOREIGN KEY clauses (cr-sqlite CRR rule).
        let sql = table_sql(&db, table).await;
        assert!(
            !sql.contains("FOREIGN KEY"),
            "{table} must have no FOREIGN KEY (cr-sqlite CRR rule); got: {sql}"
        );
    }
    for junction in ["book_authors", "book_tags", "collection_books"] {
        let sql = table_sql(&db, junction).await;
        assert!(
            !sql.contains("FOREIGN KEY"),
            "{junction} must have no FOREIGN KEY; got: {sql}"
        );
        assert!(
            sql.contains("PRIMARY KEY ("),
            "{junction} must keep its composite PRIMARY KEY; got: {sql}"
        );
    }
}

#[tokio::test]
async fn local_tables_keep_integer_id_and_refs_are_rewritten() {
    let db = setup().await;

    // sales is local (non-CRR): keeps its integer id PK, refs became uuid TEXT.
    let sales_sql = table_sql(&db, "sales").await;
    assert!(
        sales_sql.contains("\"id\" INTEGER PRIMARY KEY"),
        "sales must keep its local integer id PK; got: {sales_sql}"
    );
    assert_eq!(
        column_type(&db, "sales", "copy_id").await.to_uppercase(),
        "TEXT",
        "sales.copy_id must become uuid TEXT"
    );

    // book_notes is a MODULE table: book_id ref rewritten to uuid TEXT, proving
    // the migration sweeps module tables (its own id stays integer).
    assert_eq!(
        column_type(&db, "book_notes", "book_id")
            .await
            .to_uppercase(),
        "TEXT",
        "book_notes.book_id (module table ref) must become uuid TEXT"
    );

    // References to LOCAL tables stay integer on the rebuilt entity tables.
    assert_eq!(
        column_type(&db, "copies", "library_id")
            .await
            .to_uppercase(),
        "INTEGER",
        "copies.library_id (local ref) must stay INTEGER"
    );
}

#[tokio::test]
async fn migration_is_idempotent() {
    let db = setup().await;
    let books_sql = table_sql(&db, "books").await;

    // `run_migrations` already applied the flip; a direct re-call must detect the
    // already-migrated schema (no integer `id` on books) and be a no-op.
    db::migrate_uuid_pk(&db).await.expect("re-run is a no-op");
    db::migrate_uuid_pk(&db)
        .await
        .expect("second re-run is a no-op");

    assert_eq!(
        table_sql(&db, "books").await,
        books_sql,
        "a re-run must not touch the already-migrated schema"
    );
}

/// Pre-flight gate for the live migration: run `run_migrations` (which performs
/// the flip) against a COPY of a REAL library and assert the invariants on real
/// data: row counts preserved THROUGH the flip, no in-the-wild orphaned refs, and
/// rows in module/peer tables the synthetic schema cannot exercise.
///
/// Opt-in: set `WS2_REAL_DB` to the path of a real `.sqlite` library file. When
/// unset the test skips (never blocks CI). The original file is NEVER touched: it
/// is copied to a temp location first and the copy is migrated. Run it before the
/// live migration:
///
///   WS2_REAL_DB=/path/to/library_copy.sqlite \
///     cargo test --test uuid_pk_migration_prototype real_library_copy -- --nocapture
#[tokio::test]
async fn real_library_copy_migrates_with_invariants_preserved() {
    let Some(src) = std::env::var_os("WS2_REAL_DB") else {
        eprintln!(
            "WS2_REAL_DB not set; skipping the real-library validation gate. \
             Set it to a real library .sqlite path to run it."
        );
        return;
    };
    let src = std::path::PathBuf::from(src);
    assert!(
        src.exists(),
        "WS2_REAL_DB points at a missing file: {}",
        src.display()
    );

    // The migration is destructive, so operate on a throwaway copy.
    let dst = std::env::temp_dir().join("ws2_real_db_copy.sqlite");
    let _ = std::fs::remove_file(&dst);
    std::fs::copy(&src, &dst).expect("copy the real library DB to a temp file");

    // Pin the pool to a single connection: the rebuild toggles PRAGMA foreign_keys /
    // legacy_alter_table per-connection, so a multi-connection pool could leak an
    // FK-state mismatch across connections (see seaorm_pragma_per_connection_pool_leak).
    let url = format!("sqlite://{}?mode=rwc", dst.display());
    let mut opt = sea_orm::ConnectOptions::new(url);
    opt.max_connections(1).min_connections(1);
    let db = sea_orm::Database::connect(opt)
        .await
        .expect("open the real library copy");

    // A real phone library carries hub-directory cache rows keyed by the
    // `peer_books.peer_id = 0` sentinel (no matching `peers` row, written in a
    // dedicated FK-off window — see `upsert_directory_catalog_cache`). They are
    // FK-violating by design and predate the flip, so the migration's integrity
    // gate must NOT abort on them (aborting bricked startup on every device
    // with a populated directory cache). Seed some in case the source copy has
    // an empty cache, and assert below that they survive the flip.
    db.execute_unprepared("PRAGMA foreign_keys = OFF")
        .await
        .expect("disable FK enforcement for the sentinel seed");
    db.execute_unprepared(
        "INSERT INTO peer_books (peer_id, remote_book_id, title, synced_at, owned) VALUES \
         (0, 900001, 'Directory sentinel A', datetime('now'), 1), \
         (0, 900002, 'Directory sentinel B', datetime('now'), 1), \
         (0, 900003, 'Directory sentinel C', datetime('now'), 1)",
    )
    .await
    .expect("seed peer_id = 0 sentinel rows in the directory cache");
    db.execute_unprepared("PRAGMA foreign_keys = ON")
        .await
        .expect("re-enable FK enforcement after the sentinel seed");

    // Count rows on the ORIGINAL (old integer-id) schema, before any migration.
    let before = capture_counts(&db).await;

    // `run_migrations` forward-migrates the copy AND performs the uuid-PK flip
    // (migration 082) as its last step.
    db::run_migrations(&db)
        .await
        .expect("run_migrations (incl. the uuid-PK flip) on the real copy");

    assert_counts_preserved(&db, &before).await;
    assert_no_orphans(&db).await;

    // The by-design sentinel rows crossed the flip untouched.
    let sentinels = count_where(
        &db,
        "peer_books",
        "peer_id = 0 AND remote_book_id >= 900001",
    )
    .await;
    assert_eq!(
        sentinels, 3,
        "peer_id = 0 directory-cache sentinel rows must survive the uuid-PK flip"
    );

    let total: i64 = before.iter().map(|(_, n)| n).sum();
    eprintln!(
        "real-library copy migrated cleanly: {} rows across {} tables preserved through the flip, no orphans.",
        total,
        before.len()
    );
    let _ = std::fs::remove_file(&dst);
}
