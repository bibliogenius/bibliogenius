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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, TypeInfo, ValueRef};

use super::account_sync_engine::{
    ApplyOutcome, EntityRef, InboundChange, MergeEngine, MergeEngineError, OutboundChange,
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
    /// Per-table completeness spec, read from `pragma_table_info` on first use:
    /// the single TEXT primary key column, and the `NOT NULL` non-PK columns a
    /// changeset must carry to materialize a row that is not silently defaulted.
    /// `None` for the junction tables, whose primary key covers every column.
    row_specs: Mutex<HashMap<String, Option<RowSpec>>>,
}

/// What a complete changeset must carry for one table (see [`CrSqliteMergeEngine`]).
#[derive(Clone)]
struct RowSpec {
    pk_column: String,
    not_null_columns: Vec<String>,
}

/// cr-sqlite's `crsql_changes.cid` for the row-level delete sentinel: a tombstone
/// carries this single pseudo-column instead of the row's data columns.
const DELETE_SENTINEL_CID: &str = "-1";

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
        Self {
            db,
            row_specs: Mutex::new(HashMap::new()),
        }
    }

    /// The completeness spec of `table`, read once and cached. Returns `None` for a
    /// table whose primary key covers every column (the junctions: nothing can be
    /// defaulted there) or for any name outside the replicated set.
    async fn row_spec(&self, table: &str) -> Result<Option<RowSpec>, MergeEngineError> {
        if let Some(cached) = self.row_specs.lock().unwrap().get(table) {
            return Ok(cached.clone());
        }
        // `table` is interpolated below, so it must come from the fixed replicated
        // set and never straight off the wire.
        if !crate::infrastructure::crsqlite_crr::CRR_TABLES.contains(&table) {
            return Ok(None);
        }
        let pool = self.db.get_sqlite_connection_pool();
        let rows = sqlx::query(&format!(
            "SELECT name, \"notnull\", pk FROM pragma_table_info('{table}')"
        ))
        .fetch_all(pool)
        .await
        .map_err(err)?;

        let mut pk_columns: Vec<String> = Vec::new();
        let mut not_null_columns: Vec<String> = Vec::new();
        for row in &rows {
            let name: String = row.try_get("name").map_err(err)?;
            let is_pk: i64 = row.try_get("pk").map_err(err)?;
            let not_null: i64 = row.try_get("notnull").map_err(err)?;
            if is_pk > 0 {
                pk_columns.push(name);
            } else if not_null == 1 {
                not_null_columns.push(name);
            }
        }
        let spec = match (pk_columns.len(), not_null_columns.is_empty()) {
            (1, false) => Some(RowSpec {
                pk_column: pk_columns.remove(0),
                not_null_columns,
            }),
            _ => None,
        };
        self.row_specs
            .lock()
            .unwrap()
            .insert(table.to_owned(), spec.clone());
        Ok(spec)
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

        // Pass 1 — WHICH entities this device changed since the watermark. The
        // `site_id` filter belongs here and only here: we re-push what we authored,
        // never an entity we merely received (no echo loop).
        let changed_rows = sqlx::query(
            "SELECT DISTINCT \"table\" AS tbl, pk FROM crsql_changes \
             WHERE db_version > ? AND site_id IS crsql_site_id()",
        )
        .bind(since)
        .fetch_all(pool)
        .await
        .map_err(err)?;
        if changed_rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut changed: HashSet<(String, Vec<u8>)> = HashSet::with_capacity(changed_rows.len());
        for row in changed_rows {
            changed.insert((
                row.try_get("tbl").map_err(err)?,
                row.try_get("pk").map_err(err)?,
            ));
        }

        // Pass 2 — the FULL column set of those entities (ADR-056). The lane store
        // keeps one blob per (entity, device) and overwrites it on every push, so a
        // blob that carried only the columns above the watermark would overwrite the
        // only complete copy of the row and leave a late-bootstrapping device with a
        // changeset it cannot apply on an empty database. This pass is deliberately
        // unfiltered on both `db_version` and `site_id`: the blob must carry the
        // whole row, including columns last written long before the watermark and
        // columns authored by another device.
        //
        // It IS narrowed to the tables that actually changed. `crsql_changes` spans
        // every CRR, so without this an edit to one book would read the clock rows of
        // the whole library on every cycle, which the constrained-device budget does
        // not allow. Correctness does not depend on the predicate being pushed down
        // into the virtual table: unfiltered rows are dropped by the `changed` lookup
        // below either way.
        let tables: BTreeSet<&str> = changed.iter().map(|(t, _)| t.as_str()).collect();
        let placeholders = vec!["?"; tables.len()].join(", ");
        let sql = format!(
            "SELECT \"table\" AS tbl, pk, cid, val, col_version, db_version, site_id, cl, seq \
             FROM crsql_changes WHERE \"table\" IN ({placeholders})"
        );
        let mut query = sqlx::query(&sql);
        for table in &tables {
            query = query.bind(*table);
        }
        let rows = query.fetch_all(pool).await.map_err(err)?;

        // Group rows per entity — keyed by (table, packed pk), since cr-sqlite's
        // `crsql_changes` spans every CRR and the same packed pk can recur across
        // tables. Deterministic order via the BTreeMap.
        let mut grouped: BTreeMap<(String, Vec<u8>), Vec<ChangeRow>> = Default::default();
        for row in rows {
            let pk: Vec<u8> = row.try_get("pk").map_err(err)?;
            let table: String = row.try_get("tbl").map_err(err)?;
            if !changed.contains(&(table.clone(), pk.clone())) {
                continue;
            }
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

        // The lane HLC is this device's merge clock AT SEAL TIME, not the maximum
        // `db_version` inside the changeset. Both are monotonic per device, so a
        // replayed old blob is still rejected (ADR-042 §14 / ADR-044 §7), but only
        // this one advances when an unmodified entity is re-pushed. With the maximum,
        // a repaired full changeset carried the same HLC as the partial blob it was
        // meant to replace and the receiver discarded it forever (ADR-056).
        let hlc = self.local_version().await?;
        let mut out = Vec::with_capacity(grouped.len());
        for ((table, pk), change_rows) in grouped {
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

    async fn apply(&self, change: InboundChange) -> Result<ApplyOutcome, MergeEngineError> {
        let rows: Vec<ChangeRow> = rmp_serde::from_slice(&change.changeset).map_err(err)?;
        let pool = self.db.get_sqlite_connection_pool();

        // Completeness check (ADR-056). It only matters for a row this device does
        // NOT already have: merging into an existing row can only add or update
        // columns, never blank one. So the existence probe runs BEFORE the merge,
        // and a changeset that creates a row while missing a NOT NULL column is
        // reported incomplete, letting the caller keep the lane repairable.
        //
        // A tombstone is exempt: cr-sqlite carries a delete as the lone sentinel
        // column, so it legitimately holds none of the data columns and creates no
        // row to leave defaulted. Without this exemption every deletion replicated
        // to a device that never held the row would be reported incomplete.
        let is_tombstone = rows.iter().all(|r| r.cid == DELETE_SENTINEL_CID);
        let spec = match rows.first() {
            Some(first) if !is_tombstone => self.row_spec(&first.table).await?,
            _ => None,
        };
        let mut incomplete = false;
        if let Some(spec) = spec {
            let table = &rows[0].table;
            let existed = sqlx::query(&format!(
                "SELECT 1 FROM \"{table}\" WHERE \"{}\" = ? LIMIT 1",
                spec.pk_column
            ))
            .bind(&change.entity.entity_uuid)
            .fetch_optional(pool)
            .await
            .map_err(err)?
            .is_some();
            if !existed {
                let carried: HashSet<&str> = rows.iter().map(|r| r.cid.as_str()).collect();
                incomplete = spec
                    .not_null_columns
                    .iter()
                    .any(|c| !carried.contains(c.as_str()));
            }
        }

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
        Ok(ApplyOutcome {
            complete: !incomplete,
        })
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

    // The lane store keeps one blob per (entity, device) and overwrites it on every
    // push, so a changeset carrying only the columns above the watermark would
    // overwrite the only complete copy of the row. A device bootstrapping afterwards
    // would then apply it with no baseline underneath and cr-sqlite would materialize
    // the row from those columns alone, the rest falling back to the CRR-ready
    // defaults. ADR-056: the watermark selects WHICH entities to send, never which
    // columns, so a re-push of an edited entity must still carry the whole row and
    // must still be applicable to an empty database.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repushed_entity_still_carries_the_whole_row() {
        let sender = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let uuid = "019f0000-0000-7000-8000-000000000001";

        crate::models::book::ActiveModel {
            id: Set(uuid.to_owned()),
            title: Set("Martin Eden".to_owned()),
            isbn: Set(Some("9782070360000".to_owned())),
            created_at: Set("2026-06-22T15:22:47Z".to_owned()),
            updated_at: Set("2026-06-22T15:22:47Z".to_owned()),
            ..Default::default()
        }
        .insert(sender.db())
        .await
        .unwrap();

        // First sync: `since = 0` yields the whole row, and the watermark advances.
        let full = sender.changes_since(0).await.unwrap();
        assert_eq!(full.len(), 1, "the fresh book is the only outbound entity");
        let watermark = sender.local_version().await.unwrap();

        // The owner then edits one field. Only that column crosses the watermark.
        sender
            .exec(&format!(
                "UPDATE books SET isbn = '9781617294556' WHERE uuid = '{uuid}'"
            ))
            .await
            .unwrap();
        let repush = sender.changes_since(watermark).await.unwrap();
        assert_eq!(
            repush.len(),
            1,
            "only the edited book is re-sent: the watermark still selects entities"
        );
        let repush_cols: Vec<ChangeRow> = rmp_serde::from_slice(&repush[0].changeset).unwrap();
        let full_cols: Vec<ChangeRow> = rmp_serde::from_slice(&full[0].changeset).unwrap();
        assert_eq!(
            repush_cols.len(),
            full_cols.len(),
            "the re-push carries the whole row, not the columns above the watermark"
        );

        // A second device that enrolls after the edit pulls what the lane store holds
        // for this entity, with no baseline underneath it.
        let receiver = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let outcome = receiver
            .apply(InboundChange {
                entity: EntityRef {
                    entity_type: "books".to_owned(),
                    entity_uuid: uuid.to_owned(),
                },
                deleted: false,
                changeset: repush[0].changeset.clone(),
            })
            .await
            .unwrap();
        assert!(outcome.complete, "no NOT NULL column was left to a default");

        let merged = crate::models::book::Entity::find_by_id(uuid.to_owned())
            .one(receiver.db())
            .await
            .unwrap()
            .expect("the changeset materializes a book row on the receiver");
        assert_eq!(merged.isbn.as_deref(), Some("9781617294556"));
        assert_eq!(
            merged.title, "Martin Eden",
            "the title survives the re-push"
        );
        assert_eq!(
            merged.created_at, "2026-06-22T15:22:47Z",
            "created_at survives the re-push"
        );

        sender.finalize().await.unwrap();
        receiver.finalize().await.unwrap();
    }

    // Why no caller may run `crsql_finalize()` on a connection it keeps using
    // (ADR-056). The wound is invisible until the next merge, it names no cause,
    // and nothing recovers short of a new connection. A production device sat in
    // this state and lost every sync cycle: the app's lifecycle hook finalized the
    // live pool on `detached`, which on Android does not mean the process is dying.
    //
    // This pins the hazard, not a behaviour we want: it is the reason
    // `crsqlite_crr::finalize_and_close` takes the connection by value and the
    // reason `teardown_crrs` no longer finalizes. If cr-sqlite ever makes finalize
    // recoverable, this test fails and the rule can be relaxed on purpose.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_finalized_connection_can_no_longer_merge_anything() {
        let sender = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let receiver = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let first = "019f0000-0000-7000-8000-000000000006";
        let second = "019f0000-0000-7000-8000-000000000007";
        for (uuid, title) in [(first, "Martin Eden"), (second, "White Fang")] {
            crate::models::book::ActiveModel {
                id: Set(uuid.to_owned()),
                title: Set(title.to_owned()),
                created_at: Set("2026-06-22T15:22:47Z".to_owned()),
                updated_at: Set("2026-06-22T15:22:47Z".to_owned()),
                ..Default::default()
            }
            .insert(sender.db())
            .await
            .unwrap();
        }
        let changes = sender.changes_since(0).await.unwrap();
        let lane = |uuid: &str| {
            let change = changes
                .iter()
                .find(|c| c.entity.entity_uuid == uuid)
                .expect("the book is in the outbound set");
            InboundChange {
                entity: change.entity.clone(),
                deleted: change.deleted,
                changeset: change.changeset.clone(),
            }
        };

        // A first cycle merges normally and warms the connection.
        receiver.apply(lane(first)).await.expect("a warm merge");

        receiver.finalize().await.unwrap();

        let err = receiver
            .apply(lane(second))
            .await
            .expect_err("a finalized connection must not silently keep merging");
        assert!(
            err.0.contains("Failed to update CRR table information"),
            "the exact wording a wedged device reports, unchanged: {}",
            err.0
        );
        // The push leg is just as dead, under yet another unrelated wording.
        let err = receiver
            .local_version()
            .await
            .expect_err("crsql_db_version is gone too");
        assert!(err.0.contains("failed to fill db version"), "{}", err.0);

        sender.finalize().await.unwrap();
        // `receiver` is deliberately left un-dropped: its pool cannot be closed
        // cleanly once cr-sqlite's statement cache has been released.
        std::mem::forget(receiver);
    }

    // Pass 2 of `changes_since` is unfiltered on `site_id`, so a re-push carries
    // the columns another device authored back to that very device, alongside
    // columns whose `db_version` sits far below the receiver's watermark. Both
    // are new shapes on the wire since ADR-056 and both must merge cleanly: when
    // a mixed-version fleet did start failing, this is the first explanation
    // reached for, and it is wrong (see the ADR). Round-trip it so the answer
    // stays recorded in the suite rather than in a diagnosis nobody re-reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_changeset_round_trips_through_the_device_that_authored_part_of_it() {
        let a = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let b = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let uuid = "019f0000-0000-7000-8000-000000000005";

        let ship = async |from: &CrSqliteMergeEngine, to: &CrSqliteMergeEngine| {
            let changes = from.changes_since(0).await.unwrap();
            let change = changes
                .iter()
                .find(|c| c.entity.entity_uuid == uuid)
                .expect("the book is in the outbound set");
            to.apply(InboundChange {
                entity: change.entity.clone(),
                deleted: change.deleted,
                changeset: change.changeset.clone(),
            })
            .await
            .expect("a full changeset must merge")
        };

        crate::models::book::ActiveModel {
            id: Set(uuid.to_owned()),
            title: Set("Martin Eden".to_owned()),
            created_at: Set("2026-06-22T15:22:47Z".to_owned()),
            updated_at: Set("2026-06-22T15:22:47Z".to_owned()),
            ..Default::default()
        }
        .insert(a.db())
        .await
        .unwrap();
        assert!(ship(&a, &b).await.complete);

        // B authors one column, so A's clock ends up holding a row stamped with
        // B's site id.
        b.exec(&format!(
            "UPDATE books SET isbn = '9781617294556' WHERE uuid = '{uuid}'"
        ))
        .await
        .unwrap();
        assert!(ship(&b, &a).await.complete);

        // A re-pushes the whole row, B's column included, straight back to B.
        assert!(ship(&a, &b).await.complete);
        let merged = crate::models::book::Entity::find_by_id(uuid.to_owned())
            .one(b.db())
            .await
            .unwrap()
            .expect("the book survives the round trip");
        assert_eq!(merged.title, "Martin Eden");
        assert_eq!(merged.isbn.as_deref(), Some("9781617294556"));

        a.finalize().await.unwrap();
        b.finalize().await.unwrap();
    }

    // The anti-rollback floor (ADR-042 H5) rejects any blob whose HLC does not
    // advance. When the HLC was `max(db_version)` over the changeset, re-pushing an
    // unmodified entity carried the very same value, so the floor discarded exactly
    // the blob that would have repaired a row. Stamping the sender's clock at seal
    // time keeps the guarantee (an old replay is still lower) while letting a repair
    // through (ADR-056).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repush_of_an_unmodified_entity_advances_the_lane_hlc() {
        let sender = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let uuid = "019f0000-0000-7000-8000-000000000002";

        crate::models::book::ActiveModel {
            id: Set(uuid.to_owned()),
            title: Set("Martin Eden".to_owned()),
            created_at: Set("2026-06-22T15:22:47Z".to_owned()),
            updated_at: Set("2026-06-22T15:22:47Z".to_owned()),
            ..Default::default()
        }
        .insert(sender.db())
        .await
        .unwrap();
        let first = sender.changes_since(0).await.unwrap();

        // Any later local write advances the device clock. The book itself is left
        // untouched, so its own column versions do not move.
        crate::models::author::ActiveModel {
            id: Set("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b".to_owned()),
            name: Set("Jack London".to_owned()),
            created_at: Set("2026-06-29T00:00:00Z".to_owned()),
            updated_at: Set("2026-06-29T00:00:00Z".to_owned()),
        }
        .insert(sender.db())
        .await
        .unwrap();

        let repush = sender.changes_since(0).await.unwrap();
        // The two entities live in different tables, which also guards the pass-2
        // narrowing: restricting the full re-read to the changed tables must not
        // drop one of them.
        assert!(
            repush.iter().any(|c| c.entity.entity_type == "books")
                && repush.iter().any(|c| c.entity.entity_type == "authors"),
            "a push spanning two tables must carry both"
        );
        let book_again = repush
            .iter()
            .find(|c| c.entity.entity_uuid == uuid)
            .expect("the unmodified book is re-sent by a from-scratch push");
        assert!(
            book_again.hlc > first[0].hlc,
            "the repair blob must clear the floor the first push raised ({} vs {})",
            book_again.hlc,
            first[0].hlc
        );

        sender.finalize().await.unwrap();
    }

    // A sender predating ADR-056 still emits partial changesets, and blobs it already
    // wrote to the lane store outlive its upgrade. Applying one must not be silent:
    // the row is created from what the changeset carries, and the outcome reports the
    // shortfall so the caller leaves the anti-rollback floor unraised and the row
    // stays repairable.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_partial_changeset_is_applied_but_reported_incomplete() {
        let sender = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let uuid = "019f0000-0000-7000-8000-000000000003";
        crate::models::book::ActiveModel {
            id: Set(uuid.to_owned()),
            title: Set("Martin Eden".to_owned()),
            isbn: Set(Some("9782070360000".to_owned())),
            created_at: Set("2026-06-22T15:22:47Z".to_owned()),
            updated_at: Set("2026-06-22T15:22:47Z".to_owned()),
            ..Default::default()
        }
        .insert(sender.db())
        .await
        .unwrap();

        // Re-encode the changeset keeping only the columns an old sender would have
        // considered above its watermark.
        let full = sender.changes_since(0).await.unwrap();
        let kept: Vec<ChangeRow> = rmp_serde::from_slice::<Vec<ChangeRow>>(&full[0].changeset)
            .unwrap()
            .into_iter()
            .filter(|r| matches!(r.cid.as_str(), "isbn" | "updated_at"))
            .collect();
        assert_eq!(kept.len(), 2, "the stand-in partial blob keeps two columns");

        let receiver = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let outcome = receiver
            .apply(InboundChange {
                entity: EntityRef {
                    entity_type: "books".to_owned(),
                    entity_uuid: uuid.to_owned(),
                },
                deleted: false,
                changeset: rmp_serde::to_vec(&kept).unwrap(),
            })
            .await
            .unwrap();
        assert!(
            !outcome.complete,
            "a row created without its NOT NULL columns must be reported incomplete"
        );

        let merged = crate::models::book::Entity::find_by_id(uuid.to_owned())
            .one(receiver.db())
            .await
            .unwrap()
            .expect("the partial changeset is still applied");
        assert_eq!(
            merged.isbn.as_deref(),
            Some("9782070360000"),
            "what the blob does carry is kept: partial data beats no data"
        );

        // Merging into an EXISTING row can only add or update columns, so the same
        // partial changeset is not flagged the second time around.
        let again = receiver
            .apply(InboundChange {
                entity: EntityRef {
                    entity_type: "books".to_owned(),
                    entity_uuid: uuid.to_owned(),
                },
                deleted: false,
                changeset: rmp_serde::to_vec(&kept).unwrap(),
            })
            .await
            .unwrap();
        assert!(
            again.complete,
            "re-applying onto an existing row is not a loss"
        );

        sender.finalize().await.unwrap();
        receiver.finalize().await.unwrap();
    }

    // cr-sqlite carries a delete as the lone sentinel column, so a tombstone holds
    // none of the row's data columns by construction. Replicated to a device that
    // never held the row, it must NOT be mistaken for an amputated changeset: doing
    // so would log a false alarm for every deletion and leave the lane's
    // anti-rollback floor permanently unraised.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tombstone_is_not_mistaken_for_an_incomplete_changeset() {
        let sender = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let uuid = "019f0000-0000-7000-8000-000000000004";
        crate::models::book::ActiveModel {
            id: Set(uuid.to_owned()),
            title: Set("Martin Eden".to_owned()),
            created_at: Set("2026-06-22T15:22:47Z".to_owned()),
            updated_at: Set("2026-06-22T15:22:47Z".to_owned()),
            ..Default::default()
        }
        .insert(sender.db())
        .await
        .unwrap();
        sender
            .exec(&format!("DELETE FROM books WHERE uuid = '{uuid}'"))
            .await
            .unwrap();

        let out = sender.changes_since(0).await.unwrap();
        let cols: Vec<ChangeRow> = rmp_serde::from_slice(&out[0].changeset).unwrap();
        assert!(
            cols.iter().all(|c| c.cid == DELETE_SENTINEL_CID),
            "a delete travels as the sentinel column alone"
        );

        // A device that never held this book applies the tombstone.
        let receiver = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let outcome = receiver
            .apply(InboundChange {
                entity: EntityRef {
                    entity_type: "books".to_owned(),
                    entity_uuid: uuid.to_owned(),
                },
                deleted: false,
                changeset: out[0].changeset.clone(),
            })
            .await
            .unwrap();
        assert!(
            outcome.complete,
            "a tombstone creates no row, so nothing was left to a default"
        );
        assert!(
            crate::models::book::Entity::find_by_id(uuid.to_owned())
                .one(receiver.db())
                .await
                .unwrap()
                .is_none(),
            "the deletion still propagates"
        );

        sender.finalize().await.unwrap();
        receiver.finalize().await.unwrap();
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
