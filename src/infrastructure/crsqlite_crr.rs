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
}
