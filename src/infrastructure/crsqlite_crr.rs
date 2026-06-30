//! cr-sqlite CRR setup for the account-sync replicated tables.
//!
//! Turns the device-stable, uuid-PK, FK-free schema (ADR-044) into cr-sqlite
//! CRRs ("conflict-free replicated relations") so local edits are captured as
//! `crsql_changes` rows and merge across a user's devices. `crsql_as_crr`
//! requires each table to have a PRIMARY KEY and no FOREIGN KEY constraints —
//! both guaranteed by the uuid-PK migration (`migrate_uuid_pk`).
//!
//! Requires a cr-sqlite-loaded connection: the `crsqlite-static` ship build
//! (registered via `sqlite3_auto_extension`) or the `crsqlite` dynamic dev
//! path. This is NOT part of the default schema — promoting a table to a CRR
//! creates cr-sqlite's clock/trigger machinery and belongs to the account-sync
//! build only, run after migrations and behind the account-sync gate.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbErr, Statement};

/// The tables replicated across a user's devices, as CRRs (ADR-044): the seven
/// entity tables plus the three junction tables. `sales` and `book_notes` are
/// deliberately absent — they stay device-local (their references were merely
/// rewritten to uuid, they are not replicated).
///
/// This MUST stay in sync with the `crr: true` specs in
/// `db::uuid_rebuild_specs` (which makes each table CRR-ready: synthesizes
/// NOT NULL defaults and drops non-PK UNIQUE indexes). A table listed here whose
/// schema was not made CRR-ready would abort in `crsql_as_crr`; the
/// `crrs_set_up_*` test guards the coupling by running `setup_crrs` over this
/// list against the real migrated schema.
pub const CRR_TABLES: &[&str] = &[
    "books",
    "authors",
    "tags",
    "contacts",
    "copies",
    "loans",
    "collections",
    "book_authors",
    "book_tags",
    "collection_books",
];

/// Promote every replicated table to a cr-sqlite CRR. Idempotent: calling
/// `crsql_as_crr` on an already-promoted table is a no-op. Must run on a
/// cr-sqlite-loaded connection, after the schema migrations (so each table
/// exists with its uuid PK and no FK).
pub async fn setup_crrs(db: &DatabaseConnection) -> Result<(), DbErr> {
    for table in CRR_TABLES {
        // `table` is a fixed name from CRR_TABLES (never user input); SQLite
        // cannot bind an identifier into `crsql_as_crr`, so it is interpolated.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT crsql_as_crr('{table}')"),
        ))
        .await?;
    }
    Ok(())
}

/// Run `crsql_finalize()` to release cr-sqlite's per-connection state before
/// the connection is closed. cr-sqlite requires this on any connection that
/// touched a CRR, or it can abort on close. The app wires this into DB
/// shutdown (the single-connection account-sync pool finalizes once).
pub async fn finalize(db: &DatabaseConnection) -> Result<(), DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "SELECT crsql_finalize()".to_owned(),
    ))
    .await?;
    Ok(())
}

/// Whether any replicated table is currently a cr-sqlite CRR on this database,
/// detected by the presence of a `*__crsql_clock` companion table. A plain read
/// of `sqlite_master`, safe on any connection.
///
/// Used to decide whether the merge engine can run: the CRR machinery only exists
/// after [`setup_crrs`], which is gated on account enrollment plus a restart (see
/// `db::init_db_account_sync`), so a freshly-enrolled session that has not yet
/// restarted has no clock tables and cannot sync data.
pub async fn crrs_present(db: &DatabaseConnection) -> Result<bool, DbErr> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name LIKE '%\\_\\_crsql\\_clock' ESCAPE '\\') AS present"
                .to_owned(),
        ))
        .await?;
    match row {
        Some(r) => Ok(r.try_get::<i32>("", "present")? != 0),
        None => Ok(false),
    }
}

/// Demote every replicated CRR back to a plain table (`crsql_as_table`) and then
/// release cr-sqlite's per-connection state (`crsql_finalize`). After this the
/// database holds only flat tables again, with no CRR triggers calling the
/// extension: it is writable by ANY build, reversing the lock-in that
/// [`setup_crrs`] introduces. The app wires this into account logout / disable.
///
/// Idempotent and order-robust: a table that is not currently a CRR is skipped, so
/// this is safe whether or not [`setup_crrs`] ever ran on the database (e.g. an
/// enrollment that never reached its post-enrollment restart). Must run on a
/// cr-sqlite-loaded connection.
pub async fn teardown_crrs(db: &DatabaseConnection) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    for table in CRR_TABLES {
        // Only demote tables that are actually CRRs; calling `crsql_as_table` on a
        // table that was never promoted is unnecessary and keeps teardown idempotent.
        // `table` is a fixed name from CRR_TABLES (never user input).
        let clock = db
            .query_one(Statement::from_string(
                backend,
                format!(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '{table}__crsql_clock'"
                ),
            ))
            .await?;
        if clock.is_none() {
            continue;
        }
        db.execute(Statement::from_string(
            backend,
            format!("SELECT crsql_as_table('{table}')"),
        ))
        .await?;
    }
    // The CRRs are gone; release cr-sqlite's per-connection state.
    finalize(db).await
}

#[cfg(all(test, feature = "crsqlite-static"))]
mod tests {
    use super::*;
    use crate::infrastructure::crsqlite_static;
    use crate::models::author;
    use sea_orm::{ActiveModelTrait, Database, Set};

    // S5c: `crsql_as_crr` must succeed on the REAL uuid-PK, FK-free schema for
    // every replicated table, and a local edit must then surface in
    // `crsql_changes`. `register()` is process-wide, so run this isolated:
    // `cargo test --features crsqlite-static crrs_set_up`.
    #[tokio::test]
    async fn crrs_set_up_on_the_real_schema_and_capture_local_edits() {
        crsqlite_static::register();
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory");
        // Full production schema (uuid PK, FK removed, book_local extracted).
        crate::db::run_migrations(&db)
            .await
            .expect("run_migrations");

        setup_crrs(&db)
            .await
            .expect("crsql_as_crr on every replicated table");

        // A local insert into a CRR must be captured by cr-sqlite.
        author::ActiveModel {
            id: Set("a-1".to_owned()),
            name: Set("Jack London".to_owned()),
            created_at: Set("2026-06-29T00:00:00Z".to_owned()),
            updated_at: Set("2026-06-29T00:00:00Z".to_owned()),
        }
        .insert(&db)
        .await
        .expect("insert author into CRR");

        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT count(*) AS n FROM crsql_changes WHERE \"table\" = 'authors'".to_owned(),
            ))
            .await
            .expect("query crsql_changes")
            .expect("one row");
        let n: i64 = row.try_get("", "n").expect("decode count");
        assert!(
            n > 0,
            "the local author insert should be captured in crsql_changes"
        );

        finalize(&db).await.expect("crsql_finalize");
    }

    // Roundtrip: a CRR-ified DB demoted by `teardown_crrs` becomes plain tables
    // again (no `*__crsql_clock`) and still accepts writes — proving the lock-in is
    // reversed. `register()` is process-wide, so run isolated:
    // `cargo test --features crsqlite-static crr_teardown`.
    #[tokio::test]
    async fn crr_teardown_demotes_to_plain_tables_and_writes_still_work() {
        crsqlite_static::register();
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory");
        crate::db::run_migrations(&db)
            .await
            .expect("run_migrations");

        setup_crrs(&db).await.expect("setup_crrs");
        assert!(
            crrs_present(&db).await.expect("crrs_present"),
            "the schema must be CRR-ified after setup_crrs"
        );

        teardown_crrs(&db).await.expect("teardown_crrs");
        assert!(
            !crrs_present(&db).await.expect("crrs_present"),
            "no clock tables must remain after teardown"
        );

        // A write to a previously-CRR table must succeed on the flat schema (no CRR
        // trigger reaching for cr-sqlite internals).
        author::ActiveModel {
            id: Set("a-after-teardown".to_owned()),
            name: Set("George Orwell".to_owned()),
            created_at: Set("2026-06-30T00:00:00Z".to_owned()),
            updated_at: Set("2026-06-30T00:00:00Z".to_owned()),
        }
        .insert(&db)
        .await
        .expect("insert into a demoted, flat table");

        // Leave the connection clean for teardown (the static extension is loaded
        // process-wide; finalize before drop, see `static_crsqlite_loads...`).
        finalize(&db).await.expect("crsql_finalize");
    }

    // Demotion is idempotent and safe when no CRR was ever set up (an enrollment
    // that never reached its post-enrollment restart): teardown over a plain schema
    // is a no-op, and a second teardown over an already-demoted schema is too.
    #[tokio::test]
    async fn crr_teardown_is_idempotent_and_safe_without_setup() {
        crsqlite_static::register();
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory");
        crate::db::run_migrations(&db)
            .await
            .expect("run_migrations");

        // No setup_crrs ran: teardown must not error.
        teardown_crrs(&db)
            .await
            .expect("teardown over plain schema");
        assert!(!crrs_present(&db).await.expect("crrs_present"));

        // Now set up, tear down twice — the second call is a no-op.
        setup_crrs(&db).await.expect("setup_crrs");
        teardown_crrs(&db).await.expect("first teardown");
        teardown_crrs(&db)
            .await
            .expect("idempotent second teardown");
        assert!(!crrs_present(&db).await.expect("crrs_present"));
    }
}
