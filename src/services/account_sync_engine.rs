//! Account sync engine — the local sync pipeline (ST-05 Phase C1).
//!
//! Orchestrates one sync cycle: pull other devices' encrypted lanes, decrypt and
//! merge them locally, then encrypt and push our own changed entities. The pipeline
//! is engine-agnostic: it depends on three seams so it can be exercised end-to-end
//! without a database, a network, or the cr-sqlite native extension.
//!
//! - [`MergeEngine`] — produces/applies opaque per-entity changesets and exposes the
//!   local merge clock. The production impl wraps cr-sqlite (Phase C2: `crsql_as_crr`,
//!   `crsql_changes`, `db_version`); the tests use an in-memory LWW engine.
//! - [`LaneTransport`] — push/pull lanes against the hub. The production impl wraps
//!   [`AccountSyncClient`]; the tests use an in-memory stateful hub.
//! - [`SyncStateStore`] — persists the pull cursor (hub `change_seq`) and the push
//!   watermark (local `db_version`). The production impl is SQLite (migration 080);
//!   the tests use an in-memory store.
//!
//! The entity type/uuid and the merge clock live INSIDE the encrypted blob (the hub's
//! `opaque_id` is a non-invertible HMAC), so the receiver learns what to apply only
//! after decrypting — consistent with ADR-042 §6.

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use ed25519_dalek::VerifyingKey;

use crate::crypto::account_keys::AccountKeyBundle;
use crate::crypto::device_registry::{DeviceEntry, DeviceRegistry};
use crate::services::account_sync_client::{
    AccountSyncClient, LanePush, PullResponse, PushResponse, RegistryResponse,
    decode_blob_standard, encode_b64url, encode_blob_standard,
};

/// Hub pull page size (the hub caps at 200).
const PULL_PAGE_LIMIT: u32 = 200;

/// Hub per-push lane cap (`AccountSyncController::MAX_LANES_PER_PUSH`).
const MAX_LANES_PER_PUSH: usize = 500;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SyncError {
    Transport(String),
    Crypto(String),
    Merge(String),
    State(String),
    Encoding(String),
    /// Device-registry verification/adoption failed (bad signature, wrong account, or a
    /// rollback / replay attempt).
    Registry(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "Transport error: {e}"),
            Self::Crypto(e) => write!(f, "Crypto error: {e}"),
            Self::Merge(e) => write!(f, "Merge error: {e}"),
            Self::State(e) => write!(f, "Sync state error: {e}"),
            Self::Encoding(e) => write!(f, "Encoding error: {e}"),
            Self::Registry(e) => write!(f, "Device registry error: {e}"),
        }
    }
}

impl std::error::Error for SyncError {}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Identifies one synced entity (the lane the hub keys by its opaque id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub entity_type: String,
    pub entity_uuid: String,
}

/// A local change to push: the engine's current changeset for one entity.
#[derive(Debug, Clone)]
pub struct OutboundChange {
    pub entity: EntityRef,
    pub deleted: bool,
    /// Opaque changeset bytes (cr-sqlite `crsql_changes` rows for this entity).
    pub changeset: Vec<u8>,
}

/// A remote change pulled from another device's lane, decrypted, to apply locally.
#[derive(Debug, Clone)]
pub struct InboundChange {
    pub entity: EntityRef,
    pub deleted: bool,
    pub changeset: Vec<u8>,
}

/// Context for a sync cycle. `account_id`/`device_id` are bound into the blob AAD.
#[derive(Debug, Clone)]
pub struct SyncContext {
    /// Opaque hub account id (also bound into the blob AAD).
    pub account_id: String,
    /// This device's lane key (base64url), also used to exclude our own lanes on pull.
    pub device_id: String,
    /// Verified signed device registry for H3 enforcement: pulled lanes whose
    /// `device_id` is absent are ignored (ADR-043 H3). `None` accepts all lanes
    /// (e.g. before the registry has been fetched); clients SHOULD set it once known.
    pub authorized_devices: Option<DeviceRegistry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub applied: usize,
    pub pushed: usize,
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MergeEngineError(pub String);

/// Local merge engine. cr-sqlite in production; an in-memory LWW store in tests.
#[async_trait]
pub trait MergeEngine: Send + Sync {
    /// Current local merge clock (cr-sqlite `db_version`), monotonic.
    async fn local_version(&self) -> std::result::Result<i64, MergeEngineError>;
    /// Entities changed strictly after `since`, with their current changeset.
    async fn changes_since(
        &self,
        since: i64,
    ) -> std::result::Result<Vec<OutboundChange>, MergeEngineError>;
    /// Apply a remote changeset; the engine merges (field-level LWW, OR-Set, tombstones).
    async fn apply(&self, change: InboundChange) -> std::result::Result<(), MergeEngineError>;
}

/// Hub lane transport. Wraps [`AccountSyncClient`] in production; in-memory in tests.
#[async_trait]
pub trait LaneTransport: Send + Sync {
    async fn push(
        &self,
        device_id: &str,
        lanes: &[LanePush],
    ) -> std::result::Result<PushResponse, SyncError>;
    async fn pull(
        &self,
        device_id: &str,
        cursor: i64,
        limit: u32,
    ) -> std::result::Result<PullResponse, SyncError>;
    /// Fetch the opaque signed device registry (H3). `blob` is `None` if never published.
    async fn fetch_registry(&self) -> std::result::Result<RegistryResponse, SyncError>;
    /// Publish a new opaque signed registry blob (standard base64); returns the hub's
    /// new server-side `registry_seq` (informational — the signed seq inside the blob is
    /// the source of truth for anti-rollback).
    async fn publish_registry(&self, blob_b64: &str) -> std::result::Result<i64, SyncError>;
}

#[async_trait]
impl LaneTransport for AccountSyncClient {
    async fn push(
        &self,
        device_id: &str,
        lanes: &[LanePush],
    ) -> std::result::Result<PushResponse, SyncError> {
        AccountSyncClient::push(self, device_id, lanes)
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))
    }

    async fn pull(
        &self,
        device_id: &str,
        cursor: i64,
        limit: u32,
    ) -> std::result::Result<PullResponse, SyncError> {
        AccountSyncClient::pull(self, device_id, cursor, limit)
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))
    }

    async fn fetch_registry(&self) -> std::result::Result<RegistryResponse, SyncError> {
        AccountSyncClient::get_registry(self)
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))
    }

    async fn publish_registry(&self, blob_b64: &str) -> std::result::Result<i64, SyncError> {
        AccountSyncClient::post_registry(self, blob_b64)
            .await
            .map_err(|e| SyncError::Transport(e.to_string()))
    }
}

/// Persists the per-account sync cursors. SQLite in production; in-memory in tests.
#[async_trait]
pub trait SyncStateStore: Send + Sync {
    /// Hub `change_seq` high-water mark consumed so far (0 = full bootstrap).
    async fn pull_cursor(&self, account_id: &str) -> std::result::Result<i64, SyncError>;
    async fn set_pull_cursor(
        &self,
        account_id: &str,
        cursor: i64,
    ) -> std::result::Result<(), SyncError>;
    /// Local `db_version` up to which our own changes were already pushed.
    async fn push_version(&self, account_id: &str) -> std::result::Result<i64, SyncError>;
    async fn set_push_version(
        &self,
        account_id: &str,
        version: i64,
    ) -> std::result::Result<(), SyncError>;
    /// Last adopted signed-registry `registry_seq` (0 = none adopted yet). The anti-
    /// rollback floor passed to [`DeviceRegistry::adopt`].
    async fn registry_seq(&self, account_id: &str) -> std::result::Result<i64, SyncError>;
    async fn set_registry_seq(
        &self,
        account_id: &str,
        seq: i64,
    ) -> std::result::Result<(), SyncError>;
}

// ---------------------------------------------------------------------------
// Sealed blob framing (the encrypted plaintext)
// ---------------------------------------------------------------------------

/// What is encrypted into a lane blob. The entity identity and the deletion flag are
/// INSIDE the ciphertext (the hub's opaque_id is non-invertible), alongside the
/// engine's changeset which itself carries the merge clock.
#[derive(Debug, Serialize, Deserialize)]
struct LaneBlob {
    /// entity type.
    t: String,
    /// entity uuid.
    u: String,
    /// deleted (tombstone) flag.
    d: bool,
    /// opaque changeset bytes.
    c: Vec<u8>,
}

fn decode_opaque_id(b64url: &str) -> std::result::Result<[u8; 32], SyncError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(b64url)
        .map_err(|e| SyncError::Encoding(format!("bad opaque_id: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| SyncError::Encoding("opaque_id is not 32 bytes".to_string()))
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// Run one full sync cycle: pull + apply remote lanes, then push local changes.
///
/// Idempotent across cycles via the persisted cursors: pull resumes from the hub
/// `change_seq` watermark, push resends only entities changed after the local
/// `db_version` watermark. Safe to call repeatedly (markDirty / periodic / on-resume).
pub async fn sync_once(
    transport: &dyn LaneTransport,
    engine: &dyn MergeEngine,
    bundle: &AccountKeyBundle,
    state: &dyn SyncStateStore,
    ctx: &SyncContext,
) -> std::result::Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    let account_aad = ctx.account_id.as_bytes();

    // 1. PULL + apply other devices' lanes, paging until the cursor stops moving.
    let mut cursor = state.pull_cursor(&ctx.account_id).await?;
    loop {
        let resp = transport
            .pull(&ctx.device_id, cursor, PULL_PAGE_LIMIT)
            .await?;
        if resp.lanes.is_empty() {
            break;
        }

        for lane in &resp.lanes {
            // H3: ignore lanes from devices absent from the signed registry. A
            // malicious hub cannot forge the registry (signed by account_auth_sk),
            // so it cannot smuggle a lane from an unauthorized/revoked device.
            if ctx
                .authorized_devices
                .as_ref()
                .is_some_and(|reg| !reg.is_authorized(&lane.device_id))
            {
                continue;
            }
            // A blob-less tombstone (blob GC'd by the hub) cannot be applied: the
            // opaque_id is non-invertible, so we have no entity ref. Skip it.
            let Some(blob_b64) = lane.blob.as_deref() else {
                continue;
            };
            let oid = decode_opaque_id(&lane.opaque_id)?;
            let blob =
                decode_blob_standard(blob_b64).map_err(|e| SyncError::Crypto(e.to_string()))?;
            // The sender bound its OWN device_id into the AAD at seal time; pull
            // reports it as lane.device_id.
            let plaintext = Zeroizing::new(
                bundle
                    .open_entity(account_aad, &oid, lane.device_id.as_bytes(), &blob)
                    .map_err(|e| SyncError::Crypto(e.to_string()))?,
            );
            let frame: LaneBlob = rmp_serde::from_slice(&plaintext)
                .map_err(|e| SyncError::Encoding(format!("bad lane frame: {e}")))?;
            engine
                .apply(InboundChange {
                    entity: EntityRef {
                        entity_type: frame.t,
                        entity_uuid: frame.u,
                    },
                    deleted: frame.d,
                    changeset: frame.c,
                })
                .await
                .map_err(|e| SyncError::Merge(e.0))?;
            stats.applied += 1;
        }

        let advanced = resp.next_cursor > cursor;
        cursor = resp.next_cursor;
        state.set_pull_cursor(&ctx.account_id, cursor).await?;
        // Stop when the page was short or the cursor did not advance (defends against
        // a hub that returns a non-increasing next_cursor).
        if resp.lanes.len() < PULL_PAGE_LIMIT as usize || !advanced {
            break;
        }
    }

    // 2. PUSH our own changed entities since the last pushed db_version.
    let since = state.push_version(&ctx.account_id).await?;
    let current_version = engine
        .local_version()
        .await
        .map_err(|e| SyncError::Merge(e.0))?;
    let changes = engine
        .changes_since(since)
        .await
        .map_err(|e| SyncError::Merge(e.0))?;

    if !changes.is_empty() {
        let mut lanes = Vec::with_capacity(changes.len());
        for change in changes {
            let oid = bundle.opaque_id(&change.entity.entity_type, &change.entity.entity_uuid);
            let frame = LaneBlob {
                t: change.entity.entity_type,
                u: change.entity.entity_uuid,
                d: change.deleted,
                c: change.changeset,
            };
            let plaintext = Zeroizing::new(
                rmp_serde::to_vec(&frame)
                    .map_err(|e| SyncError::Encoding(format!("frame encode: {e}")))?,
            );
            let blob = bundle
                .seal_entity(account_aad, &oid, ctx.device_id.as_bytes(), &plaintext)
                .map_err(|e| SyncError::Crypto(e.to_string()))?;
            lanes.push(LanePush {
                opaque_id: encode_b64url(&oid),
                deleted: change.deleted,
                size_bucket: blob.len() as i64,
                blob: Some(encode_blob_standard(&blob)),
            });
        }
        stats.pushed = lanes.len();
        // The hub caps each push at MAX_LANES_PER_PUSH; a first sync of an existing
        // library easily exceeds it, so push in batches.
        for batch in lanes.chunks(MAX_LANES_PER_PUSH) {
            transport.push(&ctx.device_id, batch).await?;
        }
    }

    // Advance the push watermark to the version we observed before pushing, so a
    // concurrent local edit during the push is re-sent next cycle (never skipped).
    state
        .set_push_version(&ctx.account_id, current_version)
        .await?;

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Device registry (H3): fetch/adopt and enroll
// ---------------------------------------------------------------------------

/// Fetch the signed registry from the hub and adopt it: verify the account signature and
/// reject a cross-account or rolled-back registry (anti-rollback compares against the
/// persisted signed `registry_seq`, NEVER the hub's server-side counter). Returns `None`
/// if the hub has no registry yet. Does NOT persist — the caller decides which seq becomes
/// the new floor (the adopted one on refresh, or the bumped one after enroll publishes).
async fn fetch_and_adopt(
    transport: &dyn LaneTransport,
    state: &dyn SyncStateStore,
    account_id: &str,
    account_key: &VerifyingKey,
) -> std::result::Result<Option<DeviceRegistry>, SyncError> {
    let Some(blob_b64) = transport.fetch_registry().await?.blob else {
        return Ok(None);
    };
    let blob = decode_blob_standard(&blob_b64).map_err(|e| SyncError::Crypto(e.to_string()))?;
    let last_seen = state.registry_seq(account_id).await?.max(0) as u64;
    DeviceRegistry::adopt(&blob, account_key, account_id, last_seen)
        .map(Some)
        .map_err(|e| SyncError::Registry(e.to_string()))
}

/// Fetch the signed device registry from the hub, adopt it, and persist the adopted
/// signed `registry_seq`. Returns the verified registry to populate
/// [`SyncContext::authorized_devices`] before [`sync_once`] (so H3 is enforceable);
/// `None` means the hub has no registry yet (e.g. before the first device signs up).
pub async fn refresh_authorized_devices(
    transport: &dyn LaneTransport,
    state: &dyn SyncStateStore,
    account_id: &str,
    account_key: &VerifyingKey,
) -> std::result::Result<Option<DeviceRegistry>, SyncError> {
    let Some(reg) = fetch_and_adopt(transport, state, account_id, account_key).await? else {
        return Ok(None);
    };
    state
        .set_registry_seq(account_id, reg.registry_seq as i64)
        .await?;
    Ok(Some(reg))
}

/// One full sync cycle: refresh the signed device registry (H3) FIRST, then run
/// the data sync with that registry bound into the context.
///
/// Centralizes the "always refresh authorized devices before `sync_once`"
/// invariant (ADR-043 H3) in a single place, so no caller can sync against a
/// stale registry. Note: if the hub serves no registry the refresh yields
/// `None` and `sync_once` then accepts all lanes (H3 disabled, see
/// [`SyncContext::authorized_devices`]); Phase E should decide whether a `None`
/// from an already-enrolled device must mean "deny" rather than "accept all".
/// This is the seam the Phase F entrypoint
/// (`account_sync_now_ffi`) and the future periodic/on-resume triggers call;
/// the production merge `engine` is supplied by the caller (Phase E / C2-prod).
pub async fn refresh_then_sync(
    transport: &dyn LaneTransport,
    engine: &dyn MergeEngine,
    bundle: &AccountKeyBundle,
    state: &dyn SyncStateStore,
    account_id: &str,
    device_id: &str,
) -> std::result::Result<SyncStats, SyncError> {
    let authorized_devices =
        refresh_authorized_devices(transport, state, account_id, &bundle.verifying_key()).await?;
    let ctx = SyncContext {
        account_id: account_id.to_string(),
        device_id: device_id.to_string(),
        authorized_devices,
    };
    sync_once(transport, engine, bundle, state, &ctx).await
}

/// Enroll `new_device` into the account's signed registry: fetch the current registry,
/// adopt it (so we always extend the latest signed version, never a stale one), append
/// the device, bump the signed `registry_seq`, re-sign with the account key, and publish.
/// Persists the new seq and returns the updated registry.
///
/// Only an already-authorized device (it holds the trousseau / account signing key) can
/// do this; the hub stores the blob opaquely and cannot forge or reorder it. Returns
/// [`SyncError::Registry`] if the hub has no registry yet (the first one is created at
/// signup, not here).
pub async fn enroll_device(
    transport: &dyn LaneTransport,
    state: &dyn SyncStateStore,
    bundle: &AccountKeyBundle,
    account_id: &str,
    new_device: DeviceEntry,
) -> std::result::Result<DeviceRegistry, SyncError> {
    let current = fetch_and_adopt(transport, state, account_id, &bundle.verifying_key())
        .await?
        .ok_or_else(|| SyncError::Registry("no registry to extend".to_string()))?;

    let updated = current.with_device(new_device);
    let signed = updated
        .sign(&bundle.signing_key())
        .map_err(|e| SyncError::Crypto(e.to_string()))?;
    transport
        .publish_registry(&encode_blob_standard(&signed))
        .await?;
    // Persist the seq we just signed so our own publish is not seen as a rollback on the
    // next refresh (the hub's returned counter is not the signed seq, so we ignore it).
    state
        .set_registry_seq(account_id, updated.registry_seq as i64)
        .await?;
    Ok(updated)
}

// ---------------------------------------------------------------------------
// SQLite-backed sync state (migration 080)
// ---------------------------------------------------------------------------

/// Production [`SyncStateStore`] over the `account_sync_state` table (migration 080).
pub struct DbSyncStateStore {
    db: DatabaseConnection,
}

impl DbSyncStateStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    async fn column(&self, account_id: &str, col: &str) -> std::result::Result<i64, SyncError> {
        let sql = format!("SELECT {col} AS v FROM account_sync_state WHERE account_id = ?",);
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                sql,
                [account_id.into()],
            ))
            .await
            .map_err(|e| SyncError::State(e.to_string()))?;
        match row {
            Some(r) => r
                .try_get::<i64>("", "v")
                .map_err(|e| SyncError::State(e.to_string())),
            None => Ok(0),
        }
    }

    async fn upsert(
        &self,
        account_id: &str,
        col: &str,
        value: i64,
    ) -> std::result::Result<(), SyncError> {
        // `col` is one of two compile-time-fixed literals, never user input.
        let sql = format!(
            "INSERT INTO account_sync_state (account_id, {col}, last_synced_at) \
             VALUES (?, ?, datetime('now')) \
             ON CONFLICT(account_id) DO UPDATE SET {col} = excluded.{col}, \
             last_synced_at = datetime('now')",
        );
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                sql,
                [account_id.into(), value.into()],
            ))
            .await
            .map_err(|e| SyncError::State(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl SyncStateStore for DbSyncStateStore {
    async fn pull_cursor(&self, account_id: &str) -> std::result::Result<i64, SyncError> {
        self.column(account_id, "pull_cursor").await
    }
    async fn set_pull_cursor(
        &self,
        account_id: &str,
        cursor: i64,
    ) -> std::result::Result<(), SyncError> {
        self.upsert(account_id, "pull_cursor", cursor).await
    }
    async fn push_version(&self, account_id: &str) -> std::result::Result<i64, SyncError> {
        self.column(account_id, "push_version").await
    }
    async fn set_push_version(
        &self,
        account_id: &str,
        version: i64,
    ) -> std::result::Result<(), SyncError> {
        self.upsert(account_id, "push_version", version).await
    }
    async fn registry_seq(&self, account_id: &str) -> std::result::Result<i64, SyncError> {
        self.column(account_id, "registry_seq").await
    }
    async fn set_registry_seq(
        &self,
        account_id: &str,
        seq: i64,
    ) -> std::result::Result<(), SyncError> {
        self.upsert(account_id, "registry_seq", seq).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    // --- in-memory sync state ---

    #[derive(Default)]
    struct MemState {
        pull: Mutex<HashMap<String, i64>>,
        push: Mutex<HashMap<String, i64>>,
        registry: Mutex<HashMap<String, i64>>,
    }

    #[async_trait]
    impl SyncStateStore for MemState {
        async fn pull_cursor(&self, account_id: &str) -> std::result::Result<i64, SyncError> {
            Ok(*self.pull.lock().unwrap().get(account_id).unwrap_or(&0))
        }
        async fn set_pull_cursor(
            &self,
            account_id: &str,
            c: i64,
        ) -> std::result::Result<(), SyncError> {
            self.pull.lock().unwrap().insert(account_id.to_string(), c);
            Ok(())
        }
        async fn push_version(&self, account_id: &str) -> std::result::Result<i64, SyncError> {
            Ok(*self.push.lock().unwrap().get(account_id).unwrap_or(&0))
        }
        async fn set_push_version(
            &self,
            account_id: &str,
            v: i64,
        ) -> std::result::Result<(), SyncError> {
            self.push.lock().unwrap().insert(account_id.to_string(), v);
            Ok(())
        }
        async fn registry_seq(&self, account_id: &str) -> std::result::Result<i64, SyncError> {
            Ok(*self.registry.lock().unwrap().get(account_id).unwrap_or(&0))
        }
        async fn set_registry_seq(
            &self,
            account_id: &str,
            seq: i64,
        ) -> std::result::Result<(), SyncError> {
            self.registry
                .lock()
                .unwrap()
                .insert(account_id.to_string(), seq);
            Ok(())
        }
    }

    // --- in-memory stateful hub (mirrors the ADR-043 lane semantics) ---

    #[derive(Default)]
    struct HubLane {
        device_id: String,
        change_seq: i64,
        deleted: bool,
        size_bucket: i64,
        blob: Option<String>,
    }

    #[derive(Default)]
    struct MemHub {
        // key: (opaque_id, device_id)
        lanes: Mutex<HashMap<(String, String), HubLane>>,
        seq: Mutex<i64>,
        // Opaque signed registry blob (standard base64) + the hub's own monotonic counter.
        registry_blob: Mutex<Option<String>>,
        registry_seq: Mutex<i64>,
    }

    #[async_trait]
    impl LaneTransport for MemHub {
        async fn push(
            &self,
            device_id: &str,
            lanes: &[LanePush],
        ) -> std::result::Result<PushResponse, SyncError> {
            let mut store = self.lanes.lock().unwrap();
            let mut seq = self.seq.lock().unwrap();
            for lane in lanes {
                *seq += 1;
                store.insert(
                    (lane.opaque_id.clone(), device_id.to_string()),
                    HubLane {
                        device_id: device_id.to_string(),
                        change_seq: *seq,
                        deleted: lane.deleted,
                        size_bucket: lane.size_bucket,
                        blob: lane.blob.clone(),
                    },
                );
            }
            Ok(PushResponse {
                accepted: lanes.len() as u32,
                high_change_seq: *seq,
            })
        }

        async fn pull(
            &self,
            device_id: &str,
            cursor: i64,
            limit: u32,
        ) -> std::result::Result<PullResponse, SyncError> {
            let store = self.lanes.lock().unwrap();
            let mut rows: Vec<(&(String, String), &HubLane)> = store
                .iter()
                .filter(|(_, l)| l.change_seq > cursor && l.device_id != device_id)
                .collect();
            rows.sort_by_key(|(_, l)| l.change_seq);
            rows.truncate(limit as usize);
            let mut next = cursor;
            let lanes = rows
                .iter()
                .map(|((oid, _), l)| {
                    next = next.max(l.change_seq);
                    crate::services::account_sync_client::LanePull {
                        opaque_id: oid.clone(),
                        device_id: l.device_id.clone(),
                        change_seq: l.change_seq,
                        deleted: l.deleted,
                        size_bucket: l.size_bucket,
                        blob: l.blob.clone(),
                    }
                })
                .collect();
            Ok(PullResponse {
                lanes,
                next_cursor: next,
            })
        }

        async fn fetch_registry(&self) -> std::result::Result<RegistryResponse, SyncError> {
            Ok(RegistryResponse {
                blob: self.registry_blob.lock().unwrap().clone(),
                registry_seq: *self.registry_seq.lock().unwrap(),
            })
        }

        async fn publish_registry(&self, blob_b64: &str) -> std::result::Result<i64, SyncError> {
            // The hub stores the blob opaquely and bumps its own counter (no CAS).
            let mut seq = self.registry_seq.lock().unwrap();
            *seq += 1;
            *self.registry_blob.lock().unwrap() = Some(blob_b64.to_string());
            Ok(*seq)
        }
    }

    // --- in-memory entity-level LWW merge engine ---

    #[derive(Clone)]
    struct Record {
        value: String,
        // Hybrid clock: (counter, device) compared lexicographically.
        hlc: (i64, String),
        deleted: bool,
        // local db_version when this record last changed locally.
        local_version: i64,
    }

    #[derive(Serialize, Deserialize)]
    struct FakeChangeset {
        value: String,
        hlc_counter: i64,
        hlc_device: String,
    }

    struct FakeEngine {
        device: String,
        clock: Mutex<i64>,
        version: Mutex<i64>,
        store: Mutex<HashMap<String, Record>>, // uuid -> record
    }

    impl FakeEngine {
        fn new(device: &str) -> Self {
            Self {
                device: device.to_string(),
                clock: Mutex::new(0),
                version: Mutex::new(0),
                store: Mutex::new(HashMap::new()),
            }
        }

        /// Local edit (book entity), bumping both the HLC and the local version.
        fn edit(&self, uuid: &str, value: &str, deleted: bool) {
            let mut clock = self.clock.lock().unwrap();
            *clock += 1;
            let mut version = self.version.lock().unwrap();
            *version += 1;
            self.store.lock().unwrap().insert(
                uuid.to_string(),
                Record {
                    value: value.to_string(),
                    hlc: (*clock, self.device.clone()),
                    deleted,
                    local_version: *version,
                },
            );
        }

        fn snapshot(&self) -> Vec<(String, String, bool)> {
            let mut out: Vec<_> = self
                .store
                .lock()
                .unwrap()
                .iter()
                .map(|(k, r)| (k.clone(), r.value.clone(), r.deleted))
                .collect();
            out.sort();
            out
        }
    }

    #[async_trait]
    impl MergeEngine for FakeEngine {
        async fn local_version(&self) -> std::result::Result<i64, MergeEngineError> {
            Ok(*self.version.lock().unwrap())
        }

        async fn changes_since(
            &self,
            since: i64,
        ) -> std::result::Result<Vec<OutboundChange>, MergeEngineError> {
            let store = self.store.lock().unwrap();
            let mut out = Vec::new();
            for (uuid, rec) in store.iter() {
                if rec.local_version > since {
                    let cs = FakeChangeset {
                        value: rec.value.clone(),
                        hlc_counter: rec.hlc.0,
                        hlc_device: rec.hlc.1.clone(),
                    };
                    out.push(OutboundChange {
                        entity: EntityRef {
                            entity_type: "book".to_string(),
                            entity_uuid: uuid.clone(),
                        },
                        deleted: rec.deleted,
                        changeset: rmp_serde::to_vec(&cs).unwrap(),
                    });
                }
            }
            Ok(out)
        }

        async fn apply(&self, change: InboundChange) -> std::result::Result<(), MergeEngineError> {
            let cs: FakeChangeset = rmp_serde::from_slice(&change.changeset)
                .map_err(|e| MergeEngineError(e.to_string()))?;
            let incoming = (cs.hlc_counter, cs.hlc_device.clone());
            let mut clock = self.clock.lock().unwrap();
            // Advance our clock past anything we have seen (HLC receive rule).
            *clock = (*clock).max(cs.hlc_counter);
            drop(clock);

            let mut store = self.store.lock().unwrap();
            let take = match store.get(&change.entity.entity_uuid) {
                Some(existing) => incoming > existing.hlc, // last-write-wins
                None => true,
            };
            if take {
                // Applying a remote change must NOT bump local_version, or it would
                // be re-pushed as if it were a local edit (echo loop).
                store.insert(
                    change.entity.entity_uuid.clone(),
                    Record {
                        value: cs.value,
                        hlc: incoming,
                        deleted: change.deleted,
                        local_version: 0,
                    },
                );
            }
            Ok(())
        }
    }

    fn ctx(device: &str) -> SyncContext {
        SyncContext {
            account_id: "acct-1".to_string(),
            device_id: device.to_string(),
            authorized_devices: None,
        }
    }

    /// Build a registry authorizing exactly `device_ids` (the in-memory H3 check uses
    /// `is_authorized` directly; sign/verify is covered in the device_registry tests).
    fn registry_for(device_ids: &[&str]) -> DeviceRegistry {
        use crate::crypto::device_registry::DeviceEntry;
        let devices = device_ids
            .iter()
            .map(|id| DeviceEntry {
                device_id: id.to_string(),
                ed25519_pk: [0u8; 32],
                x25519_pk: [0u8; 32],
                name: id.to_string(),
            })
            .collect();
        DeviceRegistry {
            account_id: "acct-1".to_string(),
            registry_seq: 1,
            devices,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_devices_converge_after_offline_edits() {
        // Shared account bundle (both devices unwrapped the same trousseau) + shared hub.
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());

        let eng_a = FakeEngine::new("devA");
        let eng_b = FakeEngine::new("devB");
        let state_a = MemState::default();
        let state_b = MemState::default();
        let ctx_a = ctx("devA");
        let ctx_b = ctx("devB");

        // Offline divergence: both edit the same book differently; B also adds another.
        eng_a.edit("book-1", "title from A", false);
        eng_b.edit("book-1", "title from B", false);
        eng_b.edit("book-2", "only on B", false);

        // Exchange: two rounds so each side both pushes and ingests the other.
        for _ in 0..2 {
            sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx_a)
                .await
                .unwrap();
            sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx_b)
                .await
                .unwrap();
            sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx_a)
                .await
                .unwrap();
        }

        let snap_a = eng_a.snapshot();
        let snap_b = eng_b.snapshot();
        assert_eq!(snap_a, snap_b, "devices must converge (A+B == B+A)");
        // book-1 resolves to B's edit (higher HLC counter: B edited after A here is a
        // tie on counter=1, broken by device id "devB" > "devA").
        let book1 = snap_a.iter().find(|(u, _, _)| u == "book-1").unwrap();
        assert_eq!(book1.1, "title from B");
        // book-2 propagated to A.
        assert!(snap_a.iter().any(|(u, _, _)| u == "book-2"));
    }

    #[tokio::test]
    async fn pushed_blob_is_ciphertext_not_plaintext() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng = FakeEngine::new("devA");
        let state = MemState::default();

        let secret = "SUPER SECRET BOOK TITLE";
        eng.edit("book-1", secret, false);
        sync_once(&*hub, &eng, &bundle, &state, &ctx("devA"))
            .await
            .unwrap();

        let store = hub.lanes.lock().unwrap();
        let lane = store.values().next().expect("a lane was pushed");
        let blob = decode_blob_standard(lane.blob.as_deref().unwrap()).unwrap();
        assert!(
            blob.windows(secret.len()).all(|w| w != secret.as_bytes()),
            "plaintext leaked into the pushed lane blob"
        );
    }

    #[tokio::test]
    async fn cursors_make_resync_idempotent() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = FakeEngine::new("devA");
        let eng_b = FakeEngine::new("devB");
        let state_a = MemState::default();
        let state_b = MemState::default();

        eng_a.edit("book-1", "from A", false);
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();

        // B pulls A's lane once.
        let first = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        assert_eq!(first.applied, 1);
        // A second sync with no new remote lanes applies nothing (cursor advanced).
        let second = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        assert_eq!(second.applied, 0);
    }

    #[tokio::test]
    async fn own_lanes_are_not_pulled_back() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng = FakeEngine::new("devA");
        let state = MemState::default();

        eng.edit("book-1", "from A", false);
        // First cycle pushes book-1.
        let s1 = sync_once(&*hub, &eng, &bundle, &state, &ctx("devA"))
            .await
            .unwrap();
        assert_eq!(s1.pushed, 1);
        // Second cycle: nothing new to push, and our own lane must not come back.
        let s2 = sync_once(&*hub, &eng, &bundle, &state, &ctx("devA"))
            .await
            .unwrap();
        assert_eq!(s2.applied, 0);
        assert_eq!(s2.pushed, 0);
    }

    #[tokio::test]
    async fn h3_drops_lanes_from_unauthorized_devices() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());

        // devA is authorized; devX is not in the signed registry.
        let eng_a = FakeEngine::new("devA");
        let eng_x = FakeEngine::new("devX");
        let state_a = MemState::default();
        let state_x = MemState::default();
        eng_a.edit("book-a", "from A", false);
        eng_x.edit("book-x", "from X (rogue)", false);
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();
        sync_once(&*hub, &eng_x, &bundle, &state_x, &ctx("devX"))
            .await
            .unwrap();

        // devB pulls with a registry authorizing only devB + devA (devX excluded, H3).
        let eng_b = FakeEngine::new("devB");
        let state_b = MemState::default();
        let mut ctx_b = ctx("devB");
        ctx_b.authorized_devices = Some(registry_for(&["devA", "devB"]));

        sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx_b)
            .await
            .unwrap();

        let snap = eng_b.snapshot();
        assert!(
            snap.iter().any(|(u, _, _)| u == "book-a"),
            "authorized lane applied"
        );
        assert!(
            !snap.iter().any(|(u, _, _)| u == "book-x"),
            "lane from an unauthorized device must be dropped (H3)"
        );
    }

    #[tokio::test]
    async fn db_sync_state_store_roundtrips() {
        use sea_orm::{ConnectOptions, Database};

        // Single pooled connection so the in-memory DB persists across queries.
        let mut opts = ConnectOptions::new("sqlite::memory:");
        opts.max_connections(1);
        let db = Database::connect(opts).await.unwrap();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE account_sync_state (account_id TEXT PRIMARY KEY, \
             pull_cursor INTEGER NOT NULL DEFAULT 0, \
             push_version INTEGER NOT NULL DEFAULT 0, \
             registry_seq INTEGER NOT NULL DEFAULT 0, last_synced_at TEXT)"
                .to_owned(),
        ))
        .await
        .unwrap();

        let store = DbSyncStateStore::new(db);

        // Unknown account defaults to 0.
        assert_eq!(store.pull_cursor("acct-1").await.unwrap(), 0);
        assert_eq!(store.push_version("acct-1").await.unwrap(), 0);
        assert_eq!(store.registry_seq("acct-1").await.unwrap(), 0);

        // Insert path, then read back.
        store.set_pull_cursor("acct-1", 7).await.unwrap();
        store.set_push_version("acct-1", 12).await.unwrap();
        store.set_registry_seq("acct-1", 3).await.unwrap();
        assert_eq!(store.pull_cursor("acct-1").await.unwrap(), 7);
        assert_eq!(store.push_version("acct-1").await.unwrap(), 12);
        assert_eq!(store.registry_seq("acct-1").await.unwrap(), 3);

        // Upsert path: updating one column leaves the others intact (ON CONFLICT).
        store.set_pull_cursor("acct-1", 9).await.unwrap();
        assert_eq!(store.pull_cursor("acct-1").await.unwrap(), 9);
        assert_eq!(store.push_version("acct-1").await.unwrap(), 12);
        assert_eq!(store.registry_seq("acct-1").await.unwrap(), 3);

        // Distinct accounts are isolated.
        assert_eq!(store.pull_cursor("acct-2").await.unwrap(), 0);
    }

    // --- device registry: fetch/adopt + enroll ---

    /// Sign `reg` with the account key and publish it to the in-memory hub.
    async fn seed_registry(hub: &MemHub, reg: &DeviceRegistry, bundle: &AccountKeyBundle) {
        let signed = reg.sign(&bundle.signing_key()).unwrap();
        hub.publish_registry(&encode_blob_standard(&signed))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn refresh_adopts_registry_and_persists_signed_seq() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        let mut reg = registry_for(&["devA"]);
        reg.registry_seq = 4;
        seed_registry(&hub, &reg, &bundle).await;

        let adopted = refresh_authorized_devices(&hub, &state, "acct-1", &bundle.verifying_key())
            .await
            .unwrap()
            .expect("registry present");

        assert!(adopted.is_authorized("devA"));
        // The SIGNED seq (4) is persisted, not the hub's own counter (1 after one publish).
        assert_eq!(state.registry_seq("acct-1").await.unwrap(), 4);

        // A second refresh of the same registry is idempotent (seq == last_seen is allowed).
        assert!(
            refresh_authorized_devices(&hub, &state, "acct-1", &bundle.verifying_key())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn refresh_returns_none_when_hub_has_no_registry() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        let got = refresh_authorized_devices(&hub, &state, "acct-1", &bundle.verifying_key())
            .await
            .unwrap();
        assert!(got.is_none());
        assert_eq!(state.registry_seq("acct-1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn refresh_rejects_a_rolled_back_registry() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        // We have already adopted seq 5; the hub serves an older validly-signed seq-2.
        state.set_registry_seq("acct-1", 5).await.unwrap();
        let mut stale = registry_for(&["devA"]);
        stale.registry_seq = 2;
        seed_registry(&hub, &stale, &bundle).await;

        let err = refresh_authorized_devices(&hub, &state, "acct-1", &bundle.verifying_key())
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Registry(_)));
        // The rollback attempt must not lower our persisted floor.
        assert_eq!(state.registry_seq("acct-1").await.unwrap(), 5);
    }

    #[tokio::test]
    async fn refresh_rejects_a_foreign_account_signature() {
        let bundle = AccountKeyBundle::generate();
        let attacker = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        seed_registry(&hub, &registry_for(&["devA"]), &bundle).await;

        // Verifying against a different account key must fail (a malicious hub forgery).
        let err = refresh_authorized_devices(&hub, &state, "acct-1", &attacker.verifying_key())
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Registry(_)));
    }

    #[tokio::test]
    async fn enroll_appends_device_and_republishes_signed_registry() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        seed_registry(&hub, &registry_for(&["devA"]), &bundle).await; // seq 1

        let new_device = DeviceEntry {
            device_id: "devB".to_string(),
            ed25519_pk: [9u8; 32],
            x25519_pk: [8u8; 32],
            name: "new phone".to_string(),
        };
        let updated = enroll_device(&hub, &state, &bundle, "acct-1", new_device)
            .await
            .unwrap();
        assert_eq!(updated.registry_seq, 2);
        assert!(updated.is_authorized("devA") && updated.is_authorized("devB"));
        assert_eq!(state.registry_seq("acct-1").await.unwrap(), 2);

        // The republished blob on the hub verifies and carries both devices.
        let resp = hub.fetch_registry().await.unwrap();
        let blob = decode_blob_standard(&resp.blob.unwrap()).unwrap();
        let published = DeviceRegistry::verify(&blob, &bundle.verifying_key()).unwrap();
        assert!(published.is_authorized("devB"));
        assert_eq!(published.registry_seq, 2);
    }

    #[tokio::test]
    async fn enroll_without_existing_registry_errors() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        let new_device = DeviceEntry {
            device_id: "devB".to_string(),
            ed25519_pk: [0u8; 32],
            x25519_pk: [0u8; 32],
            name: "new".to_string(),
        };
        let err = enroll_device(&hub, &state, &bundle, "acct-1", new_device)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Registry(_)));
    }

    #[tokio::test]
    async fn refresh_then_sync_refreshes_registry_before_syncing() {
        // The registry authorizes devA + devB only. An UNREGISTERED devX pushes a
        // lane straight to the hub. refresh_then_sync must adopt the registry
        // first, so the H3 filter drops devX's lane within the same cycle.
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        seed_registry(&hub, &registry_for(&["devA", "devB"]), &bundle).await;

        let eng_x = FakeEngine::new("devX");
        let state_x = MemState::default();
        eng_x.edit("book-x", "from an unauthorized device", false);
        sync_once(&hub, &eng_x, &bundle, &state_x, &ctx("devX"))
            .await
            .unwrap();

        // devA syncs without any pre-set context: the registry is fetched inside
        // the cycle, so devX is filtered and nothing is applied.
        let eng_a = FakeEngine::new("devA");
        let stats = refresh_then_sync(&hub, &eng_a, &bundle, &state, "acct-1", "devA")
            .await
            .unwrap();
        assert_eq!(
            stats.applied, 0,
            "unauthorized devX lane must be filtered by the refreshed registry"
        );
        // The signed seq is persisted, proving the refresh ran in this cycle.
        assert_eq!(state.registry_seq("acct-1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn refresh_then_sync_applies_an_authorized_lane() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        seed_registry(&hub, &registry_for(&["devA", "devB"]), &bundle).await;

        // devB is authorized; its lane must flow through to devA via the cycle.
        let eng_b = FakeEngine::new("devB");
        let state_b = MemState::default();
        eng_b.edit("book-1", "from B", false);
        sync_once(&hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();

        let eng_a = FakeEngine::new("devA");
        let state_a = MemState::default();
        let stats = refresh_then_sync(&hub, &eng_a, &bundle, &state_a, "acct-1", "devA")
            .await
            .unwrap();
        assert_eq!(stats.applied, 1);
        assert!(eng_a.snapshot().iter().any(|(u, _, _)| u == "book-1"));
    }

    // C2 spike: the SAME sync_once pipeline, driven by the REAL cr-sqlite engine
    // (two in-memory cr-sqlite DBs) instead of the in-memory fake. Validates that the
    // real CRDT engine converges through our encrypt/transport/cursor loop.
    // Runs only with `--features crsqlite` (needs the vendored extension).
    #[cfg(feature = "crsqlite")]
    #[tokio::test(flavor = "multi_thread")]
    async fn real_crsqlite_two_devices_converge() {
        use crate::services::crsqlite_engine::CrSqliteMergeEngine;

        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = CrSqliteMergeEngine::open_in_memory("books").unwrap();
        let eng_b = CrSqliteMergeEngine::open_in_memory("books").unwrap();
        let state_a = MemState::default();
        let state_b = MemState::default();

        // Offline divergence on two real cr-sqlite databases.
        eng_a.upsert("book-1", "title from A").unwrap();
        eng_b.upsert("book-1", "title from B").unwrap();
        eng_b.upsert("book-2", "only on B").unwrap();

        for _ in 0..2 {
            sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
                .await
                .unwrap();
            sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
                .await
                .unwrap();
            sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
                .await
                .unwrap();
        }

        let snap_a = eng_a.snapshot().unwrap();
        let snap_b = eng_b.snapshot().unwrap();
        // cr-sqlite decides the LWW winner for book-1 by its own HLC; we only assert
        // the two real engines converge and both rows propagated.
        assert_eq!(snap_a, snap_b, "real cr-sqlite engines must converge");
        assert!(snap_a.iter().any(|(u, _)| u == "book-1"));
        assert!(snap_a.iter().any(|(u, _)| u == "book-2"));
    }
}
