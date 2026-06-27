//! Prototype: the id -> uuid primary-key migration for the account-sync merge engine.
//!
//! This is a SANDBOX, not a production migration. It does NOT touch
//! `run_migrations` and never bumps `SCHEMA_VERSION`, so it carries zero
//! regression risk. Its job is to de-risk the real migration by proving, against the REAL
//! schema (built via `run_migrations`) plus a representative fixture, that the
//! candidate migration:
//!   - rebuilds the 6 replicated entity tables to a `uuid TEXT PRIMARY KEY`
//!     (dropping the device-local integer `id`, per ADR-044 Addendum A: Option A),
//!   - rewrites every cross-entity reference from integer id to the parent's uuid,
//!   - drops FOREIGN KEY enforcement on the replicated tables (cr-sqlite forbids
//!     FK on CRRs), with referential integrity preserved (no orphans, no row loss),
//!   - keeps references to LOCAL tables (library_id, user_id, lender_peer_id) integer.
//!
//! The rebuild is GENERIC (driven by `PRAGMA table_info`) so it survives column
//! drift — the same property the real migration needs. The FK toggle runs on a
//! dedicated acquired connection (never the shared pool), mirroring the leak-safe
//! precedent at `frb.rs:5410`.
//!
//! Scope deferred to later prototype slices / the real migration (called out where relevant):
//!   - validating `crsql_as_crr` on the rebuilt schema (needs a cr-sqlite-loaded
//!     connection),
//!   - re-creating secondary indexes and the uuid-population trigger,
//!   - the FULL FK fan-out across non-core tables (only surfaces against a real
//!     library copy with data in those tables — this test reports the fan-out).

use std::collections::BTreeSet;

use rust_lib_app::db;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sqlx::Row;

/// One replicated entity / junction / local table to rebuild.
struct Spec {
    table: &'static str,
    /// Drop the integer `id` column (mode A: entities).
    drop_id: bool,
    /// Make `uuid` the PRIMARY KEY (mode A: entities).
    uuid_pk: bool,
    /// Composite PK columns (mode B: junctions); empty otherwise.
    composite: &'static [&'static str],
    /// `(column, parent_table)` references to rewrite from integer id to parent uuid.
    refs: &'static [(&'static str, &'static str)],
}

/// The migration plan. Order is irrelevant for correctness: every `_new` table is
/// populated by resolving references against the still-intact originals (phase 1),
/// and only then are the originals dropped and the `_new` tables renamed (phase 2).
fn specs() -> Vec<Spec> {
    vec![
        // Mode A: entities -> uuid PK, integer id dropped.
        Spec {
            table: "books",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[],
        },
        Spec {
            table: "authors",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[],
        },
        Spec {
            table: "tags",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[("parent_id", "tags")],
        },
        Spec {
            table: "contacts",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[],
        },
        Spec {
            table: "copies",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[("book_id", "books")],
        },
        Spec {
            table: "loans",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[("copy_id", "copies"), ("contact_id", "contacts")],
        },
        // Mode B: junctions -> composite PK of the rewritten references.
        Spec {
            table: "book_authors",
            drop_id: false,
            uuid_pk: false,
            composite: &["book_id", "author_id"],
            refs: &[("book_id", "books"), ("author_id", "authors")],
        },
        Spec {
            table: "book_tags",
            drop_id: false,
            uuid_pk: false,
            composite: &["book_id", "tag_id"],
            refs: &[("book_id", "books"), ("tag_id", "tags")],
        },
        Spec {
            table: "collection_books",
            drop_id: false,
            uuid_pk: false,
            composite: &["collection_id", "book_id"],
            refs: &[("book_id", "books")],
        },
        // Mode C: local (non-CRR) tables keeping their integer id, but referencing
        // now-uuid-keyed parents -> only their reference columns move to uuid.
        Spec {
            table: "sales",
            drop_id: false,
            uuid_pk: false,
            composite: &[],
            refs: &[("copy_id", "copies"), ("contact_id", "contacts")],
        },
        // book_notes lives in an EXTENSION MODULE (src/modules/book_notes), not in
        // db.rs -- discovered via the FK fan-out diagnostic. The real migration must sweep module
        // tables too. Open product question: should per-book notes sync (would need a
        // uuid column + CRR)? For now it stays local, ref rewritten to books.uuid.
        Spec {
            table: "book_notes",
            drop_id: false,
            uuid_pk: false,
            composite: &[],
            refs: &[("book_id", "books")],
        },
    ]
}

/// Every reference that must point at a parent's uuid after the migration, plus the
/// parent's uuid-bearing column (collections is already uuid-keyed via `id`).
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

/// The 6 replicated entity tables rebuilt to a uuid PRIMARY KEY (mode A).
const REBUILT_ENTITIES: &[&str] = &["books", "authors", "tags", "contacts", "copies", "loans"];

async fn setup() -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    db::run_migrations(&db).await.expect("run migrations");
    db
}

/// Every foreign key in the DB that points INTO a rebuilt entity table, as
/// `(child_table, child_col, parent_table)`. The real migration must rewrite each of these.
async fn fanout_into_rebuilt(conn: &mut sqlx::SqliteConnection) -> Vec<(String, String, String)> {
    let tables = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();

    let mut fanout: Vec<(String, String, String)> = Vec::new();
    for t in &tables {
        let name: String = t.get("name");
        let fks = sqlx::query(&format!("PRAGMA foreign_key_list(\"{name}\")"))
            .fetch_all(&mut *conn)
            .await
            .unwrap();
        for fk in &fks {
            let parent: String = fk.get("table");
            if REBUILT_ENTITIES.contains(&parent.as_str()) {
                let from: String = fk.get("from");
                fanout.push((name.clone(), from, parent));
            }
        }
    }
    fanout.sort();
    fanout
}

/// The references the migration plan (`ALL_REFS`) handles that point INTO the
/// rebuilt set (`collection_books.collection_id -> collections` is excluded:
/// collections is already uuid-keyed and not rebuilt).
fn handled_refs_into_rebuilt() -> BTreeSet<(String, String, String)> {
    ALL_REFS
        .iter()
        .filter(|(_, _, parent, _)| REBUILT_ENTITIES.contains(parent))
        .map(|(child, col, parent, _)| (child.to_string(), col.to_string(), parent.to_string()))
        .collect()
}

/// FK fan-out references not covered by the plan, formatted for assertions.
fn uncovered_fanout(
    fanout: &[(String, String, String)],
    handled: &BTreeSet<(String, String, String)>,
) -> Vec<String> {
    fanout
        .iter()
        .filter(|fk| !handled.contains(*fk))
        .map(|(c, col, p)| format!("{c}.{col} -> {p}"))
        .collect()
}

/// Row count per table in the migration plan, captured before the rebuild.
async fn capture_counts(db: &DatabaseConnection) -> Vec<(String, i64)> {
    let mut v = Vec::new();
    for spec in specs() {
        v.push((spec.table.to_string(), count(db, spec.table).await));
    }
    v
}

/// Assert the rebuild neither lost nor duplicated any row.
async fn assert_counts_preserved(db: &DatabaseConnection, before: &[(String, i64)]) {
    for (table, n) in before {
        assert_eq!(
            count(db, table).await,
            *n,
            "row count changed for {table}: the rebuild lost or duplicated rows"
        );
    }
}

/// Assert every rewritten reference resolves to a parent's uuid (no orphans).
async fn assert_no_orphans(db: &DatabaseConnection) {
    for (child, col, parent, parent_col) in ALL_REFS {
        let orphans = count_orphans(db, child, col, parent, parent_col).await;
        assert_eq!(
            orphans, 0,
            "{child}.{col} has {orphans} value(s) not present in {parent}.{parent_col} after the rewrite"
        );
    }
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

/// `CREATE TABLE \"<table>\"` SQL as stored by SQLite (for structural assertions).
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

/// Seed a representative slice: a book with two authors, two tags (one a child of
/// the other), two copies, a loan, a sale, and a collection membership. References
/// use explicit integer ids; the migration-078 triggers fill the uuids. Seeding runs
/// with foreign_keys OFF so we need no rows in the local parent tables
/// (libraries/users/peers), keeping the fixture free of their schema details.
async fn seed(db: &DatabaseConnection) {
    let now = chrono::Utc::now().to_rfc3339();
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.expect("acquire for seed");
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();

    let stmts = [
        format!(
            "INSERT INTO books (id, title, created_at, updated_at) VALUES (1, 'Book One', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO books (id, title, created_at, updated_at) VALUES (2, 'Book Two', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO authors (id, name, created_at, updated_at) VALUES (1, 'Author A', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO authors (id, name, created_at, updated_at) VALUES (2, 'Author B', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO tags (id, name, created_at, updated_at) VALUES (1, 'Parent', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO tags (id, name, parent_id, created_at, updated_at) VALUES (2, 'Child', 1, '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO contacts (id, type, name, library_owner_id, created_at, updated_at) VALUES (1, 'borrower', 'Alice', 1, '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO contacts (id, type, name, library_owner_id, created_at, updated_at) VALUES (2, 'borrower', 'Bob', 1, '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO copies (id, book_id, library_id, status, created_at, updated_at) VALUES (1, 1, 1, 'available', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO copies (id, book_id, library_id, status, created_at, updated_at) VALUES (2, 2, 1, 'available', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO loans (id, copy_id, contact_id, library_id, loan_date, due_date) VALUES (1, 1, 1, 1, '{now}', '{now}')"
        ),
        "INSERT INTO book_authors (book_id, author_id) VALUES (1, 1)".to_string(),
        "INSERT INTO book_authors (book_id, author_id) VALUES (1, 2)".to_string(),
        "INSERT INTO book_tags (book_id, tag_id) VALUES (1, 1)".to_string(),
        "INSERT INTO book_tags (book_id, tag_id) VALUES (2, 2)".to_string(),
        format!(
            "INSERT INTO collections (id, name, source, created_at, updated_at) VALUES ('col-1', 'My Shelf', 'manual', '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO collection_books (collection_id, book_id, added_at) VALUES ('col-1', 1, '{now}')"
        ),
        format!(
            "INSERT INTO sales (id, copy_id, contact_id, library_id, sale_date, sale_price, created_at, updated_at) VALUES (1, 2, 2, 1, '{now}', 9.5, '{now}', '{now}')"
        ),
        format!(
            "INSERT INTO book_notes (id, book_id, content, created_at, updated_at) VALUES (1, 1, 'a reading note', '{now}', '{now}')"
        ),
    ];
    for sql in stmts {
        sqlx::query(&sql)
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("seed failed: {sql}\n{e}"));
    }
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await
        .unwrap();
}

/// Read `(name, type, notnull, pk)` for every column of a table.
async fn columns(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Vec<(String, String, bool, bool)> {
    let rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
        .fetch_all(&mut *conn)
        .await
        .expect("table_info");
    rows.iter()
        .map(|r| {
            (
                r.get::<String, _>("name"),
                r.get::<String, _>("type"),
                r.get::<i64, _>("notnull") != 0,
                r.get::<i64, _>("pk") != 0,
            )
        })
        .collect()
}

/// Phase 1: build and populate `<table>__new` resolving refs against the intact original.
async fn build_new(conn: &mut sqlx::SqliteConnection, spec: &Spec) {
    let cols = columns(conn, spec.table).await;
    let mut defs: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut sel: Vec<String> = Vec::new();
    let mut joins = String::new();

    for (name, ty, notnull, pk) in &cols {
        if name == "id" && spec.drop_id {
            continue;
        }
        if name == "uuid" {
            defs.push(format!(
                "uuid TEXT NOT NULL{}",
                if spec.uuid_pk { " PRIMARY KEY" } else { "" }
            ));
            names.push("uuid".to_string());
            sel.push("t.uuid".to_string());
            continue;
        }
        if let Some((_, parent)) = spec.refs.iter().find(|(c, _)| c == name) {
            defs.push(format!(
                "\"{name}\" TEXT{}",
                if *notnull { " NOT NULL" } else { "" }
            ));
            names.push(format!("\"{name}\""));
            let alias = format!("p_{name}");
            sel.push(format!("{alias}.uuid"));
            joins.push_str(&format!(
                " LEFT JOIN \"{parent}\" {alias} ON {alias}.id = t.\"{name}\""
            ));
            continue;
        }
        // Plain column (includes the integer `id` in mode C, which keeps its PK).
        let keep_pk = *pk && !spec.drop_id && !spec.uuid_pk && spec.composite.is_empty();
        let ty = if ty.is_empty() { "TEXT" } else { ty.as_str() };
        defs.push(format!(
            "\"{name}\" {ty}{}{}",
            if *notnull { " NOT NULL" } else { "" },
            if keep_pk { " PRIMARY KEY" } else { "" }
        ));
        names.push(format!("\"{name}\""));
        sel.push(format!("t.\"{name}\""));
    }

    if !spec.composite.is_empty() {
        let pk_cols = spec
            .composite
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        defs.push(format!("PRIMARY KEY ({pk_cols})"));
    }

    let new = format!("{}__new", spec.table);
    sqlx::query(&format!("CREATE TABLE \"{new}\" ({})", defs.join(", ")))
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("create {new}: {e}"));
    sqlx::query(&format!(
        "INSERT INTO \"{new}\" ({}) SELECT {} FROM \"{}\" t{joins}",
        names.join(", "),
        sel.join(", "),
        spec.table
    ))
    .execute(&mut *conn)
    .await
    .unwrap_or_else(|e| panic!("populate {new}: {e}"));
}

/// Run the candidate migration on a dedicated connection, FK enforcement scoped off.
async fn run_migration(db: &DatabaseConnection) {
    let specs = specs();
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.expect("acquire for migration");

    // SQLite's table-redefinition procedure: FK off for the rebuild window. On the
    // real multi-connection pool, the real migration MUST keep this on a dedicated acquired
    // connection (never the shared pool) and restore ON before it returns -- the
    // frb.rs:5410 leak-safe pattern. Here the in-memory pool is single-connection.
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();
    // Modern SQLite (>= 3.25) rewrites references in other objects on RENAME, which
    // re-validates their FKs against the new schema and raises "foreign key mismatch"
    // while an original child (e.g. book_notes -> books(id)) still references the old
    // shape. legacy_alter_table disables that rewrite; the final schema is validated
    // by foreign_key_check below. This is SQLite's documented table-redefinition path.
    sqlx::query("PRAGMA legacy_alter_table = ON")
        .execute(&mut *conn)
        .await
        .unwrap();

    // Phase 1: all `_new` tables, refs resolved against intact originals.
    for spec in &specs {
        build_new(&mut conn, spec).await;
    }
    // Phase 2a: drop ALL originals first, so no surviving table references an
    // old-shaped parent when we rename.
    for spec in &specs {
        sqlx::query(&format!("DROP TABLE \"{}\"", spec.table))
            .execute(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("drop {}: {e}", spec.table));
    }
    // Phase 2b: rename `_new` into place.
    for spec in &specs {
        sqlx::query(&format!(
            "ALTER TABLE \"{}__new\" RENAME TO \"{}\"",
            spec.table, spec.table
        ))
        .execute(&mut *conn)
        .await
        .unwrap_or_else(|e| panic!("rename {}: {e}", spec.table));
    }

    sqlx::query("PRAGMA legacy_alter_table = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();

    // Integrity gate: no remaining FK is violated by the seeded rows.
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert!(
        violations.is_empty(),
        "foreign_key_check reported {} violation(s) after the rebuild",
        violations.len()
    );

    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await
        .unwrap();
    // Release the connection before any pooled query runs (single-conn pool).
    drop(conn);
}

#[tokio::test]
async fn migration_preserves_every_row() {
    let db = setup().await;
    seed(&db).await;

    let before = capture_counts(&db).await;
    run_migration(&db).await;
    assert_counts_preserved(&db, &before).await;
}

#[tokio::test]
async fn every_reference_resolves_to_a_parent_uuid() {
    let db = setup().await;
    seed(&db).await;
    run_migration(&db).await;

    assert_no_orphans(&db).await;
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

#[tokio::test]
async fn replicated_tables_have_uuid_pk_and_no_foreign_keys() {
    let db = setup().await;
    seed(&db).await;
    run_migration(&db).await;

    for table in ["books", "authors", "tags", "contacts", "copies", "loans"] {
        let sql = table_sql(&db, table).await;
        assert!(
            sql.contains("uuid TEXT NOT NULL PRIMARY KEY"),
            "{table} should have uuid as PRIMARY KEY; got: {sql}"
        );
        assert!(
            !sql.contains("FOREIGN KEY"),
            "{table} must have no FOREIGN KEY (cr-sqlite CRR rule); got: {sql}"
        );
        assert!(
            !sql.contains("\"id\" INTEGER") && !sql.contains("id INTEGER PRIMARY KEY"),
            "{table} must no longer carry the device-local integer id; got: {sql}"
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
async fn local_table_keeps_integer_id_and_local_refs_stay_integer() {
    let db = setup().await;
    seed(&db).await;
    run_migration(&db).await;

    // sales is local (non-CRR): keeps its integer id PK, but its refs to the now
    // uuid-keyed copies/contacts became TEXT.
    let sales_sql = table_sql(&db, "sales").await;
    assert!(
        sales_sql.contains("\"id\" INTEGER PRIMARY KEY"),
        "sales must keep its local integer id PK; got: {sales_sql}"
    );
    assert!(
        sales_sql.contains("\"copy_id\" TEXT"),
        "sales.copy_id must become uuid TEXT; got: {sales_sql}"
    );

    // References to LOCAL tables must stay integer on the rebuilt entity tables.
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.unwrap();
    let copies_cols = columns(&mut conn, "copies").await;
    let library_id = copies_cols
        .iter()
        .find(|(n, _, _, _)| n == "library_id")
        .expect("copies.library_id present");
    assert_eq!(
        library_id.1.to_uppercase(),
        "INTEGER",
        "copies.library_id (local ref) must stay INTEGER"
    );
}

/// Drift guard: EVERY foreign key into a rebuilt table must be covered by the
/// migration plan (`ALL_REFS`). This is what discovered `book_notes` (an
/// extension-module table) the first time; as an assertion it now fails loudly if a
/// future schema/module adds a new reference into the rebuilt set without updating
/// the plan — the exact class of omission that would corrupt the live migration.
/// (The seeded fixture only has core rows, so the *data*-level checks alone could
/// miss an unhandled-but-empty referencing table; this schema-level check cannot.)
#[tokio::test]
async fn fk_fanout_is_fully_covered_by_the_migration_plan() {
    let db = setup().await;
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.unwrap();

    let fanout = fanout_into_rebuilt(&mut conn).await;
    eprintln!(
        "FK fan-out into the rebuilt tables (the real migration must rewrite each):\n  {}",
        fanout
            .iter()
            .map(|(c, col, p)| format!("{c}.{col} -> {p}"))
            .collect::<Vec<_>>()
            .join("\n  ")
    );

    let uncovered = uncovered_fanout(&fanout, &handled_refs_into_rebuilt());
    assert!(
        uncovered.is_empty(),
        "unhandled FK(s) into the rebuilt tables — add to the migration plan (specs/ALL_REFS): {uncovered:?}"
    );
    // Sanity: the plan is not stale either (a known core ref is really present).
    assert!(
        fanout
            .iter()
            .any(|(c, col, p)| c == "copies" && col == "book_id" && p == "books"),
        "expected copies.book_id -> books in the FK fan-out"
    );
}

/// Pre-flight gate for the real migration: run the candidate migration against a COPY of a REAL library DB
/// and assert the invariants on real data: volume, NULLs, in-the-wild orphans, and
/// rows in module/peer tables the synthetic fixture does not exercise.
///
/// Opt-in: set `WS2_REAL_DB` to the path of a real `.sqlite` library file. When
/// unset the test is a no-op (skips), so it never blocks CI. The original file is
/// NEVER touched: it is copied to a temp location first and the copy is migrated.
/// Run it before the live migration:
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
    // FK-state mismatch across connections (see memory: seaorm_pragma_per_connection_pool_leak).
    let url = format!("sqlite://{}?mode=rwc", dst.display());
    let mut opt = sea_orm::ConnectOptions::new(url);
    opt.max_connections(1).min_connections(1);
    let db = sea_orm::Database::connect(opt)
        .await
        .expect("open the real library copy");

    // Forward-migrate the copy to the current schema (idempotent) so the uuid columns
    // (migration 078) and module tables the candidate migration relies on are present
    // even if the copy was taken from an older app version.
    db::run_migrations(&db)
        .await
        .expect("forward-migrate the copy to the current schema");

    // Schema drift guard on the REAL schema: every FK into a rebuilt table must be in
    // the plan. Catches a module/peer table this fixture-blind prototype never saw.
    {
        let pool = db.get_sqlite_connection_pool();
        let mut conn = pool.acquire().await.unwrap();
        let fanout = fanout_into_rebuilt(&mut conn).await;
        let uncovered = uncovered_fanout(&fanout, &handled_refs_into_rebuilt());
        assert!(
            uncovered.is_empty(),
            "real DB has unhandled FK(s) into the rebuilt tables; extend the plan before the live migration: {uncovered:?}"
        );
    }

    let before = capture_counts(&db).await;
    run_migration(&db).await; // includes the foreign_key_check integrity gate
    assert_counts_preserved(&db, &before).await;
    assert_no_orphans(&db).await;

    let total: i64 = before.iter().map(|(_, n)| n).sum();
    eprintln!(
        "real-library copy migrated cleanly: {} rows across {} tables preserved, no orphans.",
        total,
        before.len()
    );
    let _ = std::fs::remove_file(&dst);
}
