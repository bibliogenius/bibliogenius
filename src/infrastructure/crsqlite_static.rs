//! Static cr-sqlite registration (shipping path, ADR-044).
//!
//! When the `crsqlite-static` feature is on, `build.rs` links the vendored
//! cr-sqlite static archive into the binary. The archive exposes the standard
//! loadable-extension entry point `sqlite3_crsqlite_init`; this module registers
//! it with SQLite via `sqlite3_auto_extension` so that **every** connection
//! opened afterwards (the whole SeaORM/sqlx pool) exposes the `crsql_*`
//! functions and the cr-sqlite triggers.
//!
//! [`register`] MUST run once, before the first `Database::connect`, so no
//! pooled connection is opened without cr-sqlite present.
//!
//! Unlike the dynamic `crsqlite` dev path (`SqliteConnectOptions::extension`),
//! this needs no file at runtime: the extension is in the binary. iOS forbids
//! `dlopen` of a separate library, which is why static link is the shipping
//! mechanism on every platform.

use std::os::raw::{c_char, c_int};
use std::sync::Once;

use libsqlite3_sys::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};

unsafe extern "C" {
    /// cr-sqlite's loadable-extension entry point (defined in the vendored
    /// static archive linked by `build.rs`).
    ///
    /// The signature matches `sqlite3_auto_extension`'s `xEntryPoint` type as
    /// bound by libsqlite3-sys (`*mut *const c_char` for the error-message
    /// out-param), so the function can be registered directly with no cast.
    fn sqlite3_crsqlite_init(
        db: *mut sqlite3,
        pz_err_msg: *mut *const c_char,
        p_api: *const sqlite3_api_routines,
    ) -> c_int;
}

static REGISTER: Once = Once::new();

/// Register the statically-linked cr-sqlite extension as a SQLite
/// auto-extension. Idempotent (guarded by a `Once`) and process-wide: every
/// connection opened after this call has `crsql_*` available.
///
/// Call this before building the database pool (before the first
/// `Database::connect`).
pub fn register() {
    REGISTER.call_once(|| {
        // SAFETY: `sqlite3_crsqlite_init` is the cr-sqlite entry point linked
        // from the vendored static archive, declared with exactly the
        // `xEntryPoint` signature `sqlite3_auto_extension` expects. SQLite
        // stores the pointer and invokes it (with valid db/api pointers) on
        // each new connection.
        unsafe {
            sqlite3_auto_extension(Some(sqlite3_crsqlite_init));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Statement};

    // S5a spike: prove the vendored static archive links against the bundled
    // SQLite and that `sqlite3_auto_extension(sqlite3_crsqlite_init)` makes the
    // `crsql_*` functions resolve on an ordinary `Database::connect` connection.
    //
    // `register()` installs a process-wide auto-extension, so run this in
    // isolation (e.g. `cargo test --features crsqlite-static
    // static_crsqlite_*`) rather than as part of the full default suite.
    #[tokio::test]
    async fn static_crsqlite_loads_on_a_connection() {
        register();
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect in-memory");
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT crsql_db_version() AS v".to_owned(),
            ))
            .await
            .expect("crsql_db_version() should be callable")
            .expect("one row");
        let v: i64 = row.try_get("", "v").expect("decode db_version");
        assert!(v >= 0, "crsql_db_version() should be a non-negative clock");

        // cr-sqlite requires `crsql_finalize()` before a connection that touched
        // it is closed; skipping it can abort on teardown. The production wiring
        // (S5c) runs this on DB shutdown — here we run it before `db` drops so
        // the test is deterministic.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "SELECT crsql_finalize()".to_owned(),
        ))
        .await
        .expect("crsql_finalize()");
    }
}
