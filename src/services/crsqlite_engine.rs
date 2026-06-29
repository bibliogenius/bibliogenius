//! Real cr-sqlite merge engine (account-sync merge over the production DB stack).
//!
//! Implements the [`MergeEngine`](super::account_sync_engine::MergeEngine) seam over an
//! actual cr-sqlite (vlcn.io v0.16.3) database, so the sync pipeline runs against the
//! real CRDT engine, not the in-memory fake.
//!
//! The production engine wraps the **application's own** `DatabaseConnection`: the
//! library DB, opened on a cr-sqlite-loaded connection (static link +
//! `sqlite3_auto_extension`, or the dynamic dev path) with every replicated table
//! promoted to a CRR via [`crsqlite_crr::setup_crrs`](crate::infrastructure::crsqlite_crr).
//! It is multi-table: `crsql_changes` is global across all CRRs, so one engine drives
//! the whole replicated set (the seven entities + three junctions).
//!
//! cr-sqlite contract used (verified against the v0.16.3 source):
//! - `crsql_changes` columns: `table, pk, cid, val, col_version, db_version, site_id, cl, seq`.
//!   `pk` is `crsql_pack_columns(<pk cols>)` — a packed binary; [`decode_single_text_pk`]
//!   recovers the uuid for our single-TEXT-PK entity tables.
//! - locally-authored changes match `site_id IS crsql_site_id()` (so we never echo
//!   changes received from another device back into our own lane).
//! - `crsql_db_version()` is the local merge clock; [`finalize`](Self::finalize) runs
//!   `crsql_finalize()` before the connection is torn down.

use std::collections::BTreeMap;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
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
/// underlying sqlx pool has the cr-sqlite extension loaded and the replicated tables
/// promoted to CRRs.
pub struct CrSqliteMergeEngine {
    db: DatabaseConnection,
}

fn err<E: std::fmt::Display>(e: E) -> MergeEngineError {
    MergeEngineError(e.to_string())
}

impl CrSqliteMergeEngine {
    /// Wrap the application's cr-sqlite-loaded database. The caller must have
    /// registered the extension and run
    /// [`crsqlite_crr::setup_crrs`](crate::infrastructure::crsqlite_crr::setup_crrs)
    /// so the replicated tables are CRRs before any sync runs.
    ///
    /// The wrapped pool MUST be single-connection: cr-sqlite keeps per-connection
    /// state (site id, db version) and an in-memory database is per-connection, so
    /// every operation must land on the same physical connection. The caller owns
    /// pool construction (e.g. `max_connections(1)`); the engine does not enforce it.
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
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
    /// engine and must call this before dropping it; the app wires it into DB shutdown.
    pub async fn finalize(&self) -> Result<(), MergeEngineError> {
        self.exec("SELECT crsql_finalize();").await
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

        // Group rows per entity — keyed by (table, packed pk), since cr-sqlite's
        // `crsql_changes` spans every CRR and the same packed pk can recur across
        // tables. Deterministic order via the BTreeMap.
        let mut grouped: BTreeMap<(String, Vec<u8>), Vec<ChangeRow>> = Default::default();
        for row in rows {
            let pk: Vec<u8> = row.try_get("pk").map_err(err)?;
            let table: String = row.try_get("tbl").map_err(err)?;
            let change = ChangeRow {
                table: table.clone(),
                pk: pk.clone(),
                cid: row.try_get("cid").map_err(err)?,
                val: decode_any(&row, "val")?,
                col_version: row.try_get("col_version").map_err(err)?,
                db_version: row.try_get("db_version").map_err(err)?,
                site_id: row.try_get("site_id").map_err(err)?,
                cl: row.try_get("cl").map_err(err)?,
                seq: row.try_get("seq").map_err(err)?,
            };
            grouped.entry((table, pk)).or_default().push(change);
        }
        let mut out = Vec::with_capacity(grouped.len());
        for ((table, pk), change_rows) in grouped {
            // The lane HLC is the highest `db_version` across the entity's rows:
            // cr-sqlite's `db_version` is monotonic per device, so each re-push of a
            // changed entity carries a strictly higher value, which the receiver uses
            // to reject a stale replay (ADR-042 §14 / ADR-044 §7).
            let hlc = change_rows.iter().map(|r| r.db_version).max().unwrap_or(0);
            // The entity uuid is the table's single TEXT primary key (our entities);
            // for a composite/non-text PK (the junctions) fall back to an opaque hex
            // key — stable per entity, which is all the lane needs there. `repair`
            // only acts on the single-uuid entity tables, where the decode succeeds.
            let entity_uuid = decode_single_text_pk(&pk).unwrap_or_else(|| hex::encode(&pk));
            let changeset = rmp_serde::to_vec(&change_rows).map_err(err)?;
            // `deleted` stays false: a cr-sqlite delete is carried as tombstone rows
            // INSIDE the changeset (apply re-inserts them, and `repair_after_apply`
            // cascades the orphans), so the delete propagates without a lane-level
            // flag. Set this true only if the transport/hub ever needs a lane-level
            // delete signal (e.g. for GC); today nothing reads it.
            out.push(OutboundChange {
                entity: EntityRef {
                    entity_type: table,
                    entity_uuid,
                },
                deleted: false,
                changeset,
                hlc,
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

    async fn repair_after_apply(
        &self,
        entity_type: &str,
        entity_uuid: &str,
    ) -> Result<(), MergeEngineError> {
        // The replicated tables have no foreign keys (cr-sqlite forbids them), so a
        // merged-in parent deletion leaves orphan children behind. `cascade_inbound_delete`
        // acts only when the parent row is now absent (a real delete merged in), never on
        // a parent that is merely not-yet-synced, so it cannot drop an in-flight row.
        crate::infrastructure::referential_integrity::cascade_inbound_delete(
            &self.db,
            entity_type,
            entity_uuid,
        )
        .await
        .map(|_| ())
        .map_err(|e| MergeEngineError(e.to_string()))
    }
}

/// Decode cr-sqlite's packed `crsql_changes.pk` to a single TEXT primary key (our
/// entity uuid). Returns `None` for a composite or non-text PK (the junction tables),
/// where the caller falls back to an opaque hex key.
///
/// Format (cr-sqlite v0.16.3 `pack_columns`): a `u8` column count, then per column a
/// `type | (intlen << 3)` byte; for TEXT (SQLite type tag 3) an `intlen`-byte
/// big-endian length follows, then the UTF-8 bytes. The round-trip is covered by a
/// test against the real engine, so a format change on a version bump fails loudly.
fn decode_single_text_pk(packed: &[u8]) -> Option<String> {
    const SQLITE_TEXT_TAG: u8 = 3;
    let mut it = packed.iter().copied();
    if it.next()? != 1 {
        return None; // not a single-column PK
    }
    let type_byte = it.next()?;
    if type_byte & 0x07 != SQLITE_TEXT_TAG {
        return None;
    }
    let intlen = (type_byte >> 3) as usize;
    if intlen == 0 || intlen > 8 {
        return None;
    }
    let mut len: usize = 0;
    for _ in 0..intlen {
        len = (len << 8) | (it.next()? as usize);
    }
    let bytes: Vec<u8> = it.by_ref().take(len).collect();
    if bytes.len() != len {
        return None;
    }
    String::from_utf8(bytes).ok()
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

#[cfg(feature = "crsqlite")]
impl CrSqliteMergeEngine {
    /// Test/dev helper: build an in-memory cr-sqlite database with the REAL migrated
    /// schema (uuid PK, FK removed, defaults) and every replicated table promoted to a
    /// CRR, then wrap it. Pinned to one connection (an in-memory DB and cr-sqlite's
    /// per-connection state both require it). Loads the extension dynamically.
    pub async fn open_real_schema_in_memory() -> Result<Self, MergeEngineError> {
        use sea_orm::SqlxSqliteConnector;
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        // cr-sqlite's entry point is non-standard, so it must be named explicitly.
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(err)?
            .extension_with_entrypoint(
                crate::infrastructure::crsqlite_dynamic::vendored_extension_path(),
                "sqlite3_crsqlite_init",
            );
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .map_err(err)?;
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);
        crate::db::run_migrations(&db)
            .await
            .map_err(|e| MergeEngineError(format!("run_migrations: {e}")))?;
        crate::infrastructure::crsqlite_crr::setup_crrs(&db)
            .await
            .map_err(|e| MergeEngineError(format!("setup_crrs: {e}")))?;
        Ok(Self::new(db))
    }

    /// Test accessor for the wrapped connection (to seed/inspect rows via SeaORM).
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}

#[cfg(all(test, feature = "crsqlite"))]
mod tests {
    use super::*;
    use crate::services::account_sync_engine::MergeEngine;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    // The packed-pk decoder must recover the uuid cr-sqlite actually stores. Seed a
    // row, read its packed `crsql_changes.pk`, and assert the decode round-trips —
    // guarding the byte format against a cr-sqlite version bump.
    #[tokio::test(flavor = "multi_thread")]
    async fn decode_single_text_pk_round_trips_against_real_engine() {
        let eng = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        crate::models::author::ActiveModel {
            id: Set("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".to_owned()),
            name: Set("Jack London".to_owned()),
            created_at: Set("2026-06-29T00:00:00Z".to_owned()),
            updated_at: Set("2026-06-29T00:00:00Z".to_owned()),
        }
        .insert(eng.db())
        .await
        .unwrap();

        let pool = eng.db().get_sqlite_connection_pool();
        let pk: Vec<u8> = sqlx::query("SELECT pk FROM crsql_changes WHERE \"table\" = 'authors'")
            .fetch_one(pool)
            .await
            .unwrap()
            .get("pk");
        assert_eq!(
            decode_single_text_pk(&pk).as_deref(),
            Some("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
        );

        eng.finalize().await.unwrap();
    }

    // `repair_after_apply` must cascade orphan children once a parent delete has
    // merged in. The replicated tables have no FK, so a vanished book leaves its
    // copies dangling; the repair hook (via `cascade_inbound_delete`) removes them
    // only because the parent row is now absent.
    #[tokio::test(flavor = "multi_thread")]
    async fn repair_after_apply_cascades_orphan_children() {
        let eng = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let now = "2026-06-29T00:00:00Z".to_owned();

        crate::models::book::ActiveModel {
            id: Set("book-1".to_owned()),
            title: Set("Martin Eden".to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(eng.db())
        .await
        .unwrap();
        crate::models::copy::ActiveModel {
            id: Set("copy-1".to_owned()),
            book_id: Set("book-1".to_owned()),
            library_id: Set(1),
            status: Set("available".to_owned()),
            is_temporary: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(eng.db())
        .await
        .unwrap();

        // Simulate a merged-in book deletion: the row vanishes but, with FKs
        // removed, the copy is left orphaned.
        eng.exec("DELETE FROM books WHERE uuid = 'book-1'")
            .await
            .unwrap();
        let copy_exists = crate::models::copy::Entity::find_by_id("copy-1".to_owned())
            .one(eng.db())
            .await
            .unwrap()
            .is_some();
        assert!(copy_exists, "copy is orphaned before repair");

        eng.repair_after_apply("book", "book-1").await.unwrap();

        let copy_after = crate::models::copy::Entity::find_by_id("copy-1".to_owned())
            .one(eng.db())
            .await
            .unwrap();
        assert!(
            copy_after.is_none(),
            "repair must cascade-delete the orphan copy of the deleted book"
        );

        eng.finalize().await.unwrap();
    }
}
