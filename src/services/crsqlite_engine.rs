//! Real cr-sqlite merge engine (ST-05 Phase C2, WS-0).
//!
//! Implements the [`MergeEngine`](super::account_sync_engine::MergeEngine) seam from
//! C1 over an actual cr-sqlite (vlcn.io v0.16.3) database, so the C1 sync pipeline can
//! be validated against the real CRDT engine, not the in-memory fake.
//!
//! WS-0 runs that engine over the **production database stack**: an sqlx `SqlitePool`
//! with cr-sqlite loaded as a runtime extension, wrapped into a SeaORM
//! [`DatabaseConnection`]. This is the deliberate change from the earlier rusqlite
//! spike — it proves cr-sqlite composes with sqlx 0.7 + SeaORM 0.12 (the stack the app
//! actually uses), so local edits issued through SeaORM are captured by cr-sqlite and
//! the `crsql_changes` lane round-trips through our encrypt/transport/cursor loop.
//!
//! Scope and isolation (deliberate):
//! - Feature-gated behind `crsqlite`; the default build/CI needs no native extension.
//! - cr-sqlite is loaded **dynamically** at runtime (`extension_with_entrypoint`). This
//!   is the local desktop dev/test path only (macOS/Linux).
//! - The SHIPPED app cannot load a separate extension file on iOS; production (WS-5)
//!   links cr-sqlite statically and registers it in-process via
//!   `sqlite3_auto_extension`. See ADR-044 sections 2-3. This module is the
//!   engine-semantics + stack-integration step, not the production static wiring.
//!
//! cr-sqlite contract used (verified against the v0.16.3 source):
//! - `crsql_changes` columns: `table, pk, cid, val, col_version, db_version, site_id, cl, seq`.
//! - locally-authored changes match `site_id IS crsql_site_id()` (so we never echo
//!   changes we received from another device back into our own lane).
//! - `crsql_db_version()` is the local merge clock; [`finalize`](Self::finalize) runs
//!   `crsql_finalize()` before the connection is torn down.

use std::collections::BTreeMap;
use std::str::FromStr;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, SqlxSqliteConnector, Statement};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, TypeInfo, ValueRef};

use super::account_sync_engine::{
    EntityRef, InboundChange, MergeEngine, MergeEngineError, OutboundChange,
};

/// A SQLite value as carried in a `crsql_changes.val` cell (ANY-typed).
#[derive(Serialize, Deserialize)]
enum SqlVal {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// One `crsql_changes` row — the unit cr-sqlite exchanges and merges.
#[derive(Serialize, Deserialize)]
struct ChangeRow {
    table: String,
    pk: Vec<u8>,
    cid: String,
    val: SqlVal,
    col_version: i64,
    db_version: i64,
    site_id: Option<Vec<u8>>,
    cl: i64,
    seq: i64,
}

/// cr-sqlite-backed [`MergeEngine`] running over a SeaORM [`DatabaseConnection`] whose
/// underlying sqlx pool has the cr-sqlite extension loaded.
pub struct CrSqliteMergeEngine {
    db: DatabaseConnection,
    table: String,
}

/// Path to the vendored cr-sqlite dynamic library (dev/test path only; see module docs).
fn vendored_extension_path() -> String {
    format!(
        "{}/vendor/crsqlite/crsqlite.dylib",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn err<E: std::fmt::Display>(e: E) -> MergeEngineError {
    MergeEngineError(e.to_string())
}

impl CrSqliteMergeEngine {
    /// Open an in-memory cr-sqlite database with one CRR table (spike helper), backed by
    /// an sqlx pool + SeaORM connection with the extension loaded.
    pub async fn open_in_memory(table: &str) -> Result<Self, MergeEngineError> {
        // cr-sqlite's entry point is non-standard, so it must be named explicitly.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(err)?
            .extension_with_entrypoint(vendored_extension_path(), "sqlite3_crsqlite_init");
        // Single connection: an in-memory database is per-connection, and cr-sqlite's
        // db_version / crsql_changes state must live on exactly one connection. Pin it
        // open for the engine's lifetime (no idle/lifetime reaping).
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .map_err(err)?;
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
        let engine = Self {
            db,
            table: table.to_string(),
        };
        // Spike schema: a single CRR table keyed by a stable text uuid.
        engine
            .exec(&format!(
                "CREATE TABLE {table} (uuid TEXT PRIMARY KEY NOT NULL, title TEXT);"
            ))
            .await?;
        engine
            .exec(&format!("SELECT crsql_as_crr('{table}');"))
            .await?;
        Ok(engine)
    }

    /// Run a statement with no result rows through the SeaORM connection.
    async fn exec(&self, sql: &str) -> Result<(), MergeEngineError> {
        self.db
            .execute(Statement::from_string(
                self.db.get_database_backend(),
                sql.to_owned(),
            ))
            .await
            .map_err(err)?;
        Ok(())
    }

    /// Run `crsql_finalize()` before the connection is closed (cr-sqlite contract).
    ///
    /// `Drop` cannot do this here because teardown is async (sqlx). Callers hold the
    /// engine and must call this before dropping it; production teardown (WS-5) wires it
    /// into the app's DB shutdown.
    pub async fn finalize(&self) -> Result<(), MergeEngineError> {
        self.exec("SELECT crsql_finalize();").await
    }

    /// Test helper: a local last-write-wins edit (upsert) of one row, issued through
    /// SeaORM so we exercise the same write path the app uses.
    pub async fn upsert(&self, uuid: &str, title: &str) -> Result<(), MergeEngineError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                format!(
                    "INSERT INTO {t} (uuid, title) VALUES (?, ?) \
                     ON CONFLICT(uuid) DO UPDATE SET title = excluded.title",
                    t = self.table
                ),
                [uuid.into(), title.into()],
            ))
            .await
            .map_err(err)?;
        Ok(())
    }

    /// Test helper: ordered `(uuid, title)` snapshot of the live table.
    pub async fn snapshot(&self) -> Result<Vec<(String, String)>, MergeEngineError> {
        let rows = self
            .db
            .query_all(Statement::from_string(
                self.db.get_database_backend(),
                format!("SELECT uuid, title FROM {t} ORDER BY uuid", t = self.table),
            ))
            .await
            .map_err(err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push((
                r.try_get::<String>("", "uuid").map_err(err)?,
                r.try_get::<String>("", "title").map_err(err)?,
            ));
        }
        Ok(out)
    }
}

#[async_trait]
impl MergeEngine for CrSqliteMergeEngine {
    async fn local_version(&self) -> Result<i64, MergeEngineError> {
        let row = self
            .db
            .query_one(Statement::from_string(
                self.db.get_database_backend(),
                "SELECT crsql_db_version() AS v".to_owned(),
            ))
            .await
            .map_err(err)?
            .ok_or_else(|| MergeEngineError("crsql_db_version() returned no row".to_string()))?;
        row.try_get::<i64>("", "v").map_err(err)
    }

    async fn changes_since(&self, since: i64) -> Result<Vec<OutboundChange>, MergeEngineError> {
        // `crsql_changes.val` is ANY-typed, so we read it through the raw sqlx pool where
        // the dynamic column type is recoverable; SeaORM's typed `try_get` cannot.
        let pool = self.db.get_sqlite_connection_pool();
        let rows = sqlx::query(
            "SELECT \"table\" AS tbl, pk, cid, val, col_version, db_version, site_id, cl, seq \
             FROM crsql_changes WHERE db_version > ? AND site_id IS crsql_site_id()",
        )
        .bind(since)
        .fetch_all(pool)
        .await
        .map_err(err)?;

        // Group rows per entity (pk) into one changeset, in deterministic order.
        let mut grouped: BTreeMap<String, Vec<ChangeRow>> = Default::default();
        for row in rows {
            let pk: Vec<u8> = row.try_get("pk").map_err(err)?;
            let change = ChangeRow {
                table: row.try_get("tbl").map_err(err)?,
                pk: pk.clone(),
                cid: row.try_get("cid").map_err(err)?,
                val: decode_any(&row, "val")?,
                col_version: row.try_get("col_version").map_err(err)?,
                db_version: row.try_get("db_version").map_err(err)?,
                site_id: row.try_get("site_id").map_err(err)?,
                cl: row.try_get("cl").map_err(err)?,
                seq: row.try_get("seq").map_err(err)?,
            };
            grouped.entry(hex::encode(&pk)).or_default().push(change);
        }
        let mut out = Vec::with_capacity(grouped.len());
        for (uuid, change_rows) in grouped {
            let changeset = rmp_serde::to_vec(&change_rows).map_err(err)?;
            out.push(OutboundChange {
                entity: EntityRef {
                    entity_type: self.table.clone(),
                    entity_uuid: uuid,
                },
                deleted: false,
                changeset,
            });
        }
        Ok(out)
    }

    async fn apply(&self, change: InboundChange) -> Result<(), MergeEngineError> {
        let rows: Vec<ChangeRow> = rmp_serde::from_slice(&change.changeset).map_err(err)?;
        let pool = self.db.get_sqlite_connection_pool();
        let mut tx = pool.begin().await.map_err(err)?;
        for r in &rows {
            let mut q = sqlx::query(
                "INSERT INTO crsql_changes \
                 (\"table\", pk, cid, val, col_version, db_version, site_id, cl, seq) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(r.table.clone())
            .bind(r.pk.clone())
            .bind(r.cid.clone());
            // Bind the ANY-typed value with its concrete SQLite type.
            q = match &r.val {
                SqlVal::Null => q.bind(Option::<i64>::None),
                SqlVal::Int(i) => q.bind(*i),
                SqlVal::Real(f) => q.bind(*f),
                SqlVal::Text(t) => q.bind(t.clone()),
                SqlVal::Blob(b) => q.bind(b.clone()),
            };
            q.bind(r.col_version)
                .bind(r.db_version)
                .bind(r.site_id.clone())
                .bind(r.cl)
                .bind(r.seq)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
        }
        tx.commit().await.map_err(err)?;
        Ok(())
    }
}

/// Decode an ANY-typed sqlx column into our serializable [`SqlVal`].
fn decode_any(row: &SqliteRow, col: &str) -> Result<SqlVal, MergeEngineError> {
    let raw = row.try_get_raw(col).map_err(err)?;
    if raw.is_null() {
        return Ok(SqlVal::Null);
    }
    match raw.type_info().name() {
        "INTEGER" => Ok(SqlVal::Int(row.try_get::<i64, _>(col).map_err(err)?)),
        "REAL" => Ok(SqlVal::Real(row.try_get::<f64, _>(col).map_err(err)?)),
        "TEXT" => Ok(SqlVal::Text(row.try_get::<String, _>(col).map_err(err)?)),
        "BLOB" => Ok(SqlVal::Blob(row.try_get::<Vec<u8>, _>(col).map_err(err)?)),
        other => Err(MergeEngineError(format!(
            "unexpected crsql_changes.val type: {other}"
        ))),
    }
}
