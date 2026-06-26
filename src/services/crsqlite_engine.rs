//! Real cr-sqlite merge engine (ST-05 Phase C2, spike).
//!
//! Implements the [`MergeEngine`](super::account_sync_engine::MergeEngine) seam from
//! C1 over an actual cr-sqlite (vlcn.io v0.16.3) database, so the C1 sync pipeline can
//! be validated against the real CRDT engine, not the in-memory fake.
//!
//! Scope and isolation (deliberate):
//! - Feature-gated behind `crsqlite`; the default build/CI needs no native extension.
//! - Uses its own `rusqlite` connection with cr-sqlite loaded **dynamically** at
//!   runtime (`load_extension`). This is the local desktop dev/test path only.
//! - The SHIPPED app cannot load a separate extension file on iOS; production C2 must
//!   link cr-sqlite statically and register it in-process on the app's own (sqlx)
//!   connections, and that is a separate, harder integration (see ADR-044 sections
//!   2-3). This module is the engine-semantics spike, not the production wiring.
//!
//! cr-sqlite contract used (verified against the v0.16.3 source):
//! - `crsql_changes` columns: `table, pk, cid, val, col_version, db_version, site_id, cl, seq`.
//! - locally-authored changes match `site_id IS crsql_site_id()` (so we never echo
//!   changes we received from another device back into our own lane).
//! - `crsql_db_version()` is the local merge clock; `crsql_finalize()` runs on close.

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};
use serde::{Deserialize, Serialize};

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

/// cr-sqlite-backed [`MergeEngine`]. The connection is wrapped in a `Mutex` so the
/// engine is `Send + Sync` (rusqlite `Connection` is `Send` but not `Sync`).
pub struct CrSqliteMergeEngine {
    conn: Mutex<Connection>,
    table: String,
}

fn vendored_extension_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/crsqlite/crsqlite.dylib")
}

fn err<E: std::fmt::Display>(e: E) -> MergeEngineError {
    MergeEngineError(e.to_string())
}

impl CrSqliteMergeEngine {
    /// Open an in-memory cr-sqlite database with one CRR table (spike helper).
    pub fn open_in_memory(table: &str) -> Result<Self, MergeEngineError> {
        let conn = Connection::open_in_memory().map_err(err)?;
        // Load cr-sqlite dynamically (dev/test path only; see module docs).
        unsafe {
            conn.load_extension_enable().map_err(err)?;
            conn.load_extension(vendored_extension_path(), Some("sqlite3_crsqlite_init"))
                .map_err(err)?;
        }
        conn.load_extension_disable().map_err(err)?;
        // Spike schema: a single CRR table keyed by a stable text uuid.
        conn.execute_batch(&format!(
            "CREATE TABLE {table} (uuid TEXT PRIMARY KEY NOT NULL, title TEXT);"
        ))
        .map_err(err)?;
        conn.execute_batch(&format!("SELECT crsql_as_crr('{table}');"))
            .map_err(err)?;
        Ok(Self {
            conn: Mutex::new(conn),
            table: table.to_string(),
        })
    }

    /// Test helper: a local last-write-wins edit (upsert) of one row.
    pub fn upsert(&self, uuid: &str, title: &str) -> Result<(), MergeEngineError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {t} (uuid, title) VALUES (?1, ?2) \
                 ON CONFLICT(uuid) DO UPDATE SET title = excluded.title",
                t = self.table
            ),
            rusqlite::params![uuid, title],
        )
        .map_err(err)?;
        Ok(())
    }

    /// Test helper: ordered `(uuid, title)` snapshot of the live table.
    pub fn snapshot(&self) -> Result<Vec<(String, String)>, MergeEngineError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT uuid, title FROM {t} ORDER BY uuid",
                t = self.table
            ))
            .map_err(err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(err)?);
        }
        Ok(out)
    }
}

impl Drop for CrSqliteMergeEngine {
    fn drop(&mut self) {
        // crsql_finalize MUST run before the connection closes (cr-sqlite contract).
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute_batch("SELECT crsql_finalize();");
        }
    }
}

#[async_trait]
impl MergeEngine for CrSqliteMergeEngine {
    async fn local_version(&self) -> Result<i64, MergeEngineError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT crsql_db_version()", [], |r| r.get(0))
            .map_err(err)
    }

    async fn changes_since(&self, since: i64) -> Result<Vec<OutboundChange>, MergeEngineError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT \"table\", pk, cid, val, col_version, db_version, site_id, cl, seq \
                 FROM crsql_changes WHERE db_version > ?1 AND site_id IS crsql_site_id()",
            )
            .map_err(err)?;
        let rows = stmt
            .query_map([since], |row| {
                Ok(ChangeRow {
                    table: row.get(0)?,
                    pk: row.get(1)?,
                    cid: row.get(2)?,
                    val: value_ref_to_sql(row.get_ref(3)?),
                    col_version: row.get(4)?,
                    db_version: row.get(5)?,
                    site_id: row.get(6)?,
                    cl: row.get(7)?,
                    seq: row.get(8)?,
                })
            })
            .map_err(err)?;

        // Group rows per entity (pk) into one changeset, in deterministic order.
        let mut grouped: std::collections::BTreeMap<String, Vec<ChangeRow>> = Default::default();
        for r in rows {
            let r = r.map_err(err)?;
            grouped.entry(hex::encode(&r.pk)).or_default().push(r);
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
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO crsql_changes \
                     (\"table\", pk, cid, val, col_version, db_version, site_id, cl, seq) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(err)?;
            for r in &rows {
                stmt.execute(rusqlite::params![
                    r.table,
                    r.pk,
                    r.cid,
                    sql_to_value(&r.val),
                    r.col_version,
                    r.db_version,
                    r.site_id,
                    r.cl,
                    r.seq,
                ])
                .map_err(err)?;
            }
        }
        tx.commit().map_err(err)?;
        Ok(())
    }
}

fn value_ref_to_sql(v: ValueRef<'_>) -> SqlVal {
    match v {
        ValueRef::Null => SqlVal::Null,
        ValueRef::Integer(i) => SqlVal::Int(i),
        ValueRef::Real(f) => SqlVal::Real(f),
        ValueRef::Text(t) => SqlVal::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => SqlVal::Blob(b.to_vec()),
    }
}

fn sql_to_value(v: &SqlVal) -> Value {
    match v {
        SqlVal::Null => Value::Null,
        SqlVal::Int(i) => Value::Integer(*i),
        SqlVal::Real(f) => Value::Real(*f),
        SqlVal::Text(t) => Value::Text(t.clone()),
        SqlVal::Blob(b) => Value::Blob(b.clone()),
    }
}
