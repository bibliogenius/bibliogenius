//! Account sync engine — the local sync pipeline.
//!
//! Orchestrates one sync cycle: pull other devices' encrypted lanes, decrypt and
//! merge them locally, then encrypt and push our own changed entities. The pipeline
//! is engine-agnostic: it depends on three seams so it can be exercised end-to-end
//! without a database, a network, or the cr-sqlite native extension.
//!
//! - [`MergeEngine`] — produces/applies opaque per-entity changesets and exposes the
//!   local merge clock. The production impl wraps cr-sqlite (`crsql_as_crr`,
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

/// How many cycles a row lane that failed to merge is retried before this device
/// gives up on it (ADR-058). A transient cause (a full disk, a locked database, a
/// cr-sqlite connection released out from under the merge) clears within a few
/// cycles, and one that survives ten is not transient. Giving up restores exactly
/// the ADR-056 behaviour: the anti-rollback floor stays unraised, so the entity is
/// still repaired by the sender's next push, whenever that comes.
const MAX_PENDING_LANE_ATTEMPTS: i64 = 10;

/// Hard ceiling on the per-account retry queue (ADR-058). The queue holds one row
/// per lane, carrying that entity's changeset, so it is bounded by design; this cap
/// bounds it against a receiver that refuses everything (a wedged connection during
/// a bootstrap pull can produce thousands of failures in one cycle). Past the cap a
/// failing lane is logged and NOT queued, which is the pre-ADR-058 behaviour rather
/// than a loss: the floor stays unraised either way.
const MAX_PENDING_LANES: usize = 500;

/// Lane entity type carrying a custom cover's image bytes (ADR-046). cr-sqlite
/// replicates rows, not files, so a hand-photographed cover's bytes ride their
/// own lane alongside the `"book"` row lane: same opaque-id derivation, same
/// sealing, same anti-replay floor, but the payload is the JPEG and the receiver
/// writes a file instead of merging a changeset.
pub const COVER_ENTITY_TYPE: &str = "cover";

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
    /// This change's merge clock (cr-sqlite `db_version`), monotonic per sending
    /// device. Sealed into the lane blob so the receiver can reject a stale replay
    /// (ADR-042 §14 / ADR-044 §7 rollback detection).
    pub hlc: i64,
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
    /// Row lanes this device is still behind on when the cycle ends: those pulled
    /// this cycle and refused by the engine, plus those a previous cycle queued
    /// whose retry failed again (ADR-056 isolation, ADR-058 retry). Each is logged
    /// and skipped without raising the anti-rollback floor, so the rest of the
    /// cycle still converged and the entity stays repairable. Reported to the
    /// caller so a mixed-version fleet is visible instead of silent. Cover lanes
    /// are not counted here: their bytes are re-offered by the cover source every
    /// cycle, so a failed cover write retries on its own.
    pub failed: usize,
    /// `(book_uuid, file_mtime)` of every custom cover whose bytes pushed
    /// successfully this cycle (ADR-046). The caller records these in the local
    /// cover dedup state AFTER the cycle succeeds, so an unchanged cover is not
    /// re-encoded and re-uploaded on the next (e.g. periodic) sync. Empty for
    /// row-only entrypoints. Kept out of the `CoverSource` trait so the
    /// flutter_rust_bridge parser never has to materialize its impls.
    pub pushed_covers: Vec<(String, i64)>,
}

/// A row lane pulled, authenticated and decrypted, that the engine refused to
/// merge, kept locally so later cycles can retry it (ADR-058).
///
/// The pull cursor advances over a skipped lane and the sender's push watermark
/// advances too, so without this nothing ever re-delivers that entity: a transient
/// refusal would be as permanent as a definitive one. The queue is this device's
/// own copy of the lane's LAST self-contained blob (ADR-056), which is exactly
/// what the hub itself retains, so replaying it applies the same snapshot the
/// sender published rather than a diff against a state nobody kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLane {
    /// The hub's non-invertible lane id, as pulled. Half of the lane identity.
    pub opaque_id: String,
    /// The device that published this blob. The other half of the lane identity,
    /// and the AAD binding under which it was opened.
    pub device_id: String,
    pub entity: EntityRef,
    pub deleted: bool,
    pub changeset: Vec<u8>,
    /// The in-ciphertext HLC this blob carried. Re-checked against the lane floor
    /// on every retry: a queued blob that a fresher one has overtaken is dropped,
    /// never replayed (that replay is precisely the H5 rollback, ADR-042 §14).
    pub hlc: i64,
    /// Retries already spent, capped by [`MAX_PENDING_LANE_ATTEMPTS`].
    pub attempts: i64,
}

/// One custom cover's bytes to publish this cycle (ADR-046, producer side).
#[derive(Debug, Clone)]
pub struct OutboundCover {
    /// The book the cover belongs to (the lane's entity uuid).
    pub book_uuid: String,
    /// The cover image bytes, already re-encoded to fit a lane blob.
    pub bytes: Vec<u8>,
    /// Freshness clock for this cover lane (the file's mtime in seconds). It
    /// advances when the user replaces the photo, so the receiver's anti-replay
    /// floor rewrites the file only on a real change, not on every cycle.
    pub hlc: i64,
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MergeEngineError(pub String);

/// What [`MergeEngine::apply`] observed about the change it merged.
#[derive(Debug, Clone, Copy)]
pub struct ApplyOutcome {
    /// `false` when the changeset created a row without carrying every `NOT NULL`
    /// column the table declares. The missing ones fell back to the CRR-ready
    /// defaults, so the row exists, is well typed, and is wrong (ADR-056). The
    /// caller must NOT raise the anti-rollback floor for such a lane, so a later
    /// complete blob can still repair the row.
    pub complete: bool,
}

impl ApplyOutcome {
    /// The ordinary case: nothing was silently defaulted.
    pub fn complete() -> Self {
        Self { complete: true }
    }
}

/// Local merge engine. cr-sqlite in production; an in-memory LWW store in tests.
#[async_trait]
pub trait MergeEngine: Send + Sync {
    /// Current local merge clock (cr-sqlite `db_version`), monotonic.
    async fn local_version(&self) -> std::result::Result<i64, MergeEngineError>;
    /// Entities changed strictly after `since`, each with the FULL changeset of its
    /// current state. `since` selects WHICH entities to send, never which columns:
    /// a lane blob must be applicable to an empty database (ADR-056).
    async fn changes_since(
        &self,
        since: i64,
    ) -> std::result::Result<Vec<OutboundChange>, MergeEngineError>;
    /// Apply a remote changeset; the engine merges (field-level LWW, OR-Set, tombstones).
    ///
    /// Borrowed, not consumed: a lane the engine refuses is queued for retry with
    /// the very bytes it was handed (ADR-058), and the caller cannot get them back
    /// from a value it gave away. Every implementation reads the changeset (it
    /// deserializes it) rather than storing it, so ownership buys nothing.
    async fn apply(
        &self,
        change: &InboundChange,
    ) -> std::result::Result<ApplyOutcome, MergeEngineError>;

    /// Repair referential integrity right after [`MergeEngine::apply`] merged a
    /// change: if the change deleted a parent entity, remove the orphan children
    /// it left behind on this device (cascade-on-inbound-delete). Acts only when
    /// a real delete was merged in, never on a parent that is merely not-yet
    /// synced, so it cannot drop a legitimately in-flight row.
    ///
    /// Default no-op: the in-memory test store and the single-table spike have no
    /// child relationships. The production library engine overrides this to run
    /// `referential_integrity::cascade_inbound_delete` on the real schema.
    async fn repair_after_apply(
        &self,
        _entity_type: &str,
        _entity_uuid: &str,
    ) -> std::result::Result<(), MergeEngineError> {
        Ok(())
    }
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
    /// Highest in-ciphertext HLC already applied for a lane `(opaque_id, device_id)`
    /// (0 = never applied). The anti-rollback floor for H5: a pulled blob whose HLC
    /// is `<=` this value is a stale replay and must be rejected.
    async fn lane_hlc(
        &self,
        account_id: &str,
        opaque_id: &str,
        device_id: &str,
    ) -> std::result::Result<i64, SyncError>;
    async fn set_lane_hlc(
        &self,
        account_id: &str,
        opaque_id: &str,
        device_id: &str,
        hlc: i64,
    ) -> std::result::Result<(), SyncError>;

    /// Lanes an earlier cycle pulled but could not merge, oldest first, each paired
    /// with its lane's current anti-rollback floor (ADR-058). Retried at the start
    /// of every cycle, before the pull leg.
    ///
    /// The floor rides along because every entry must be re-gated on it before
    /// being replayed, and reading it per entry would be one extra query per queued
    /// lane on every cycle.
    async fn pending_lanes(
        &self,
        account_id: &str,
    ) -> std::result::Result<Vec<(PendingLane, i64)>, SyncError>;
    /// Queue a lane for retry, or replace the one already queued for the same
    /// `(opaque_id, device_id)`. One row per lane: the newest blob supersedes the
    /// older one, exactly as it does in the hub's snapshot store. The caller owns
    /// `attempts` (0 on a fresh failure, incremented on each retry).
    async fn put_pending_lane(
        &self,
        account_id: &str,
        lane: &PendingLane,
    ) -> std::result::Result<(), SyncError>;
    /// Forget a queued lane: it merged, a fresher blob overtook it, or its retries
    /// ran out.
    async fn drop_pending_lane(
        &self,
        account_id: &str,
        opaque_id: &str,
        device_id: &str,
    ) -> std::result::Result<(), SyncError>;
}

/// Produces this device's custom covers to push (ADR-046). cr-sqlite replicates
/// the `books` row (including the normalized `cover_url`) but not the cover file
/// itself, so the bytes come from here. The production impl reads the local
/// `covers/` directory; the row-only sync entrypoints use `NoopCovers`.
#[async_trait]
pub trait CoverSource: Send + Sync {
    async fn pending_covers(&self) -> std::result::Result<Vec<OutboundCover>, SyncError>;
}

/// Persists a custom cover received from another device (ADR-046). The bytes are
/// written under the book's uuid so the existing resolver finds them once the
/// already-replicated `cover_url` row lands. The production impl writes the local
/// `covers/` directory; the row-only sync entrypoints use `NoopCovers`.
#[async_trait]
pub trait CoverSink: Send + Sync {
    async fn write_cover(
        &self,
        book_uuid: &str,
        bytes: &[u8],
        hlc: i64,
    ) -> std::result::Result<(), SyncError>;
}

/// A cover source/sink that produces and persists nothing. Used by the row-only
/// entrypoints (`sync_once`, `refresh_then_sync`) and by tests/engines that do
/// not transport cover bytes.
pub struct NoopCovers {}

#[async_trait]
impl CoverSource for NoopCovers {
    async fn pending_covers(&self) -> std::result::Result<Vec<OutboundCover>, SyncError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl CoverSink for NoopCovers {
    async fn write_cover(
        &self,
        _book_uuid: &str,
        _bytes: &[u8],
        _hlc: i64,
    ) -> std::result::Result<(), SyncError> {
        Ok(())
    }
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
    /// opaque payload: a cr-sqlite changeset for a row, or the image bytes for a
    /// cover. Encoded as a msgpack `bin` (not an int array) so a large cover does
    /// not inflate ~1.5x — the difference between fitting the hub blob cap and not.
    #[serde(with = "serde_bytes")]
    c: Vec<u8>,
    /// sender merge clock (HLC). Monotonic per lane; the receiver rejects a blob
    /// whose `h` does not advance past the last applied one (anti-rollback, H5).
    h: i64,
}

fn decode_opaque_id(b64url: &str) -> std::result::Result<[u8; 32], SyncError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(b64url)
        .map_err(|e| SyncError::Encoding(format!("bad opaque_id: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| SyncError::Encoding("opaque_id is not 32 bytes".to_string()))
}

/// Frame, seal, and encode one entity (a row change or a cover) into a push lane.
/// Shared by the row-change and cover legs of the push so both seal identically.
#[allow(clippy::too_many_arguments)]
fn seal_lane(
    bundle: &AccountKeyBundle,
    account_aad: &[u8],
    device_id: &[u8],
    entity_type: String,
    entity_uuid: String,
    deleted: bool,
    payload: Vec<u8>,
    hlc: i64,
) -> std::result::Result<LanePush, SyncError> {
    let oid = bundle.opaque_id(&entity_type, &entity_uuid);
    let frame = LaneBlob {
        t: entity_type,
        u: entity_uuid,
        d: deleted,
        c: payload,
        h: hlc,
    };
    let plaintext = Zeroizing::new(
        rmp_serde::to_vec(&frame).map_err(|e| SyncError::Encoding(format!("frame encode: {e}")))?,
    );
    let blob = bundle
        .seal_entity(account_aad, &oid, device_id, &plaintext)
        .map_err(|e| SyncError::Crypto(e.to_string()))?;
    Ok(LanePush {
        opaque_id: encode_b64url(&oid),
        deleted,
        size_bucket: blob.len() as i64,
        blob: Some(encode_blob_standard(&blob)),
    })
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// Which half of a lane's merge refused it. Both are isolated on the same terms,
/// but they are not the same event and the log must not conflate them: an `Apply`
/// failure changed nothing, a `Repair` failure left the changeset merged with its
/// referential cleanup unfinished.
#[derive(Debug)]
enum LaneMergeFailure {
    Apply(String),
    Repair(String),
}

impl LaneMergeFailure {
    fn stage(&self) -> &'static str {
        match self {
            Self::Apply(_) => "merge",
            Self::Repair(_) => "post-merge referential repair",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Apply(e) | Self::Repair(e) => e,
        }
    }
}

/// Merge one inbound row lane: apply the changeset, then repair the referential
/// integrity it may have broken. Shared by the pull leg and the retry queue so a
/// replayed lane goes through exactly the same two steps as a freshly pulled one.
///
/// Takes the change by reference so the caller keeps the bytes: on refusal they go
/// into the retry queue unchanged, with no copy made on the path where the merge
/// succeeds (ADR-058).
async fn merge_lane(
    engine: &dyn MergeEngine,
    change: &InboundChange,
) -> std::result::Result<ApplyOutcome, LaneMergeFailure> {
    let outcome = engine
        .apply(change)
        .await
        .map_err(|e| LaneMergeFailure::Apply(e.0))?;
    // If this change deleted a parent entity, cascade the orphan children it left
    // behind (the database no longer enforces foreign keys since the replicated
    // tables were rebuilt without them, ADR-044).
    engine
        .repair_after_apply(&change.entity.entity_type, &change.entity.entity_uuid)
        .await
        .map_err(|e| LaneMergeFailure::Repair(e.0))?;
    Ok(outcome)
}

/// Retry the lanes an earlier cycle pulled but could not merge (ADR-058).
///
/// Runs before the pull leg, so a lane freshly refused this cycle gets its first
/// retry on the next one rather than twice in a row. Each entry is re-gated on the
/// lane's anti-rollback floor before being replayed: the floor may have moved since
/// the blob was queued (a later blob for the same lane merged), and replaying a
/// blob the floor has overtaken is exactly the rollback H5 forbids (ADR-042 §14).
///
/// Returns how many lanes are still queued afterwards, so the pull leg can enforce
/// [`MAX_PENDING_LANES`] without a second count.
///
/// Queue bookkeeping (dropping an entry, spending an attempt) is best-effort: it
/// must never abort the cycle. Propagating it would freeze every other lane of the
/// account, which is the failure ADR-056 removed, and the conditions that make a
/// local write fail are the same ones that make a merge fail. Every one of those
/// failures is self-correcting on the next cycle. Raising the anti-rollback floor
/// is NOT bookkeeping and stays fatal: it is the H5 obligation.
async fn retry_pending_lanes(
    engine: &dyn MergeEngine,
    state: &dyn SyncStateStore,
    account_id: &str,
    stats: &mut SyncStats,
) -> std::result::Result<usize, SyncError> {
    let mut still_queued = 0usize;
    for (lane, floor) in state.pending_lanes(account_id).await? {
        if lane.hlc <= floor {
            // A fresher blob for this lane merged in the meantime, so this one is
            // stale. Dropping it is not a loss: the lane blob is self-contained
            // (ADR-056), so the newer one already carries everything this one did.
            forget_pending_lane(state, account_id, &lane.opaque_id, &lane.device_id).await;
            continue;
        }
        let change = InboundChange {
            entity: lane.entity,
            deleted: lane.deleted,
            changeset: lane.changeset,
        };
        match merge_lane(engine, &change).await {
            Ok(outcome) => {
                if outcome.complete {
                    // Before the drop below, so a failure here leaves the entry
                    // queued rather than losing both the entry and the floor.
                    state
                        .set_lane_hlc(account_id, &lane.opaque_id, &lane.device_id, lane.hlc)
                        .await?;
                } else {
                    tracing::warn!(
                        entity_type = %change.entity.entity_type,
                        entity_uuid = %change.entity.entity_uuid,
                        "a retried lane applied an incomplete changeset: NOT NULL columns \
                         fell back to their defaults; the anti-rollback floor is left \
                         unraised so a complete changeset can still repair this row"
                    );
                }
                tracing::info!(
                    entity_type = %change.entity.entity_type,
                    entity_uuid = %change.entity.entity_uuid,
                    attempts = lane.attempts + 1,
                    "a lane an earlier cycle could not merge was applied on retry"
                );
                stats.applied += 1;
                // A failed drop costs one futile replay next cycle, which the
                // freshly raised floor then discards as superseded.
                forget_pending_lane(state, account_id, &lane.opaque_id, &lane.device_id).await;
            }
            Err(e) => {
                stats.failed += 1;
                let attempts = lane.attempts + 1;
                if attempts >= MAX_PENDING_LANE_ATTEMPTS {
                    // Out of retries. This is where a definitive refusal ends up,
                    // which is why the engine is not asked to tell a transient
                    // failure from a permanent one: the queue finds out by trying.
                    // Giving up returns this lane to the ADR-056 state exactly, an
                    // unraised floor waiting for the sender's next push, and says so.
                    tracing::warn!(
                        entity_type = %change.entity.entity_type,
                        entity_uuid = %change.entity.entity_uuid,
                        stage = e.stage(),
                        error = %e.message(),
                        attempts,
                        "giving up on a lane after its retries ran out; it stays behind on \
                         this device until its sender pushes that entity again"
                    );
                    forget_pending_lane(state, account_id, &lane.opaque_id, &lane.device_id).await;
                } else {
                    tracing::warn!(
                        entity_type = %change.entity.entity_type,
                        entity_uuid = %change.entity.entity_uuid,
                        stage = e.stage(),
                        error = %e.message(),
                        attempts,
                        "a queued lane failed to merge again; it stays queued for a later cycle"
                    );
                    let spent = PendingLane {
                        opaque_id: lane.opaque_id,
                        device_id: lane.device_id,
                        entity: change.entity,
                        deleted: change.deleted,
                        changeset: change.changeset,
                        hlc: lane.hlc,
                        attempts,
                    };
                    // A failed write leaves the entry with its previous attempt
                    // count, so the lane is retried once more than its budget
                    // rather than escaping it.
                    if let Err(e) = state.put_pending_lane(account_id, &spent).await {
                        tracing::warn!(
                            entity_type = %spent.entity.entity_type,
                            entity_uuid = %spent.entity.entity_uuid,
                            error = %e,
                            "could not record a spent retry attempt"
                        );
                    }
                    still_queued += 1;
                }
            }
        }
    }
    Ok(still_queued)
}

/// Drop a queued lane, tolerating a failure. See [`retry_pending_lanes`] for why
/// queue bookkeeping must not abort a cycle.
async fn forget_pending_lane(
    state: &dyn SyncStateStore,
    account_id: &str,
    opaque_id: &str,
    device_id: &str,
) {
    if let Err(e) = state
        .drop_pending_lane(account_id, opaque_id, device_id)
        .await
    {
        tracing::warn!(error = %e, "could not drop a lane from the retry queue");
    }
}

/// Run one full sync cycle: pull + apply remote lanes, then push local changes.
///
/// Idempotent across cycles via the persisted cursors: pull resumes from the hub
/// `change_seq` watermark, push resends only entities changed after the local
/// `db_version` watermark. Safe to call repeatedly (markDirty / periodic / on-resume).
///
/// Row-only: custom cover bytes are not transported. Use
/// [`refresh_then_sync_with_covers`] (or [`sync_once_with_covers`]) to also carry
/// covers (ADR-046).
pub async fn sync_once(
    transport: &dyn LaneTransport,
    engine: &dyn MergeEngine,
    bundle: &AccountKeyBundle,
    state: &dyn SyncStateStore,
    ctx: &SyncContext,
) -> std::result::Result<SyncStats, SyncError> {
    sync_once_with_covers(
        transport,
        engine,
        bundle,
        state,
        ctx,
        &NoopCovers {},
        &NoopCovers {},
    )
    .await
}

/// [`sync_once`] that also pulls/pushes custom cover bytes through the given
/// cover seams (ADR-046). A pulled lane of type [`COVER_ENTITY_TYPE`] is written
/// via `cover_sink` instead of merged into the engine; `cover_source`'s covers
/// are sealed and pushed alongside the engine's row changes.
#[allow(clippy::too_many_arguments)]
pub async fn sync_once_with_covers(
    transport: &dyn LaneTransport,
    engine: &dyn MergeEngine,
    bundle: &AccountKeyBundle,
    state: &dyn SyncStateStore,
    ctx: &SyncContext,
    cover_source: &dyn CoverSource,
    cover_sink: &dyn CoverSink,
) -> std::result::Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    let account_aad = ctx.account_id.as_bytes();

    // 0. RETRY the lanes an earlier cycle pulled but could not merge (ADR-058).
    // Nothing else re-delivers them: the pull cursor advanced over them and the
    // sender's push watermark advanced too, so a transient refusal would otherwise
    // be as permanent as a definitive one.
    let mut queued = retry_pending_lanes(engine, state, &ctx.account_id, &mut stats).await?;

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
            // H5 rollback detection (ADR-042 §14 / ADR-044 §7): the AEAD binds a blob
            // to its lane but NOT to a sequence, so a hostile hub can re-serve an
            // old-but-valid blob that still decrypts. Reject any blob whose
            // in-ciphertext HLC does not advance past the last one we applied for
            // this lane. This closes the cold-bootstrap / per-lane-regression gap
            // that the monotonic pull cursor + LWW leave open: even after the cursor
            // is reset (e.g. a forced resync) the per-lane floor persists, so a
            // stale or duplicate blob is dropped rather than rolling the entity back.
            let last_hlc = state
                .lane_hlc(&ctx.account_id, &lane.opaque_id, &lane.device_id)
                .await?;
            if frame.h <= last_hlc {
                continue;
            }
            if frame.t == COVER_ENTITY_TYPE {
                // A custom cover lane (ADR-046): the payload is the image bytes,
                // not a changeset. Write the file under the book uuid; the
                // already-replicated cover_url row points the resolver at it. The
                // HLC floor below makes this idempotent — the file is written once
                // per content change, not on every cycle.
                //
                // A cover write failure (e.g. a full disk) is isolated: log and skip
                // this lane WITHOUT raising the HLC floor, so the cover is retried on
                // the next cycle and one bad cover never blocks row convergence.
                if let Err(e) = cover_sink.write_cover(&frame.u, &frame.c, frame.h).await {
                    tracing::warn!(book = %frame.u, error = %e, "failed to write a synced cover; will retry next cycle");
                    continue;
                }
            } else {
                let entity = EntityRef {
                    entity_type: frame.t,
                    entity_uuid: frame.u,
                };
                // A lane this device cannot merge is isolated, exactly as a cover
                // lane is: log it, skip it WITHOUT raising the anti-rollback floor,
                // and carry on with the cycle. Propagating the error instead would
                // abort the whole cycle before the pull cursor advances, so a single
                // unappliable entity would freeze every other lane of the account for
                // good. The engine can refuse a merge for reasons this pipeline
                // cannot anticipate (a changeset the local schema does not fit, a
                // cr-sqlite connection whose per-connection state was released), and
                // a mixed-version fleet is the normal state for users, so a refusal
                // is an expected event rather than an exceptional one. A repair
                // failure counts too: the changeset landed, but the entity is left in
                // a state this cycle could not finish.
                //
                // The skip is deliberately loud. Silence is what let ADR-056's
                // divergence run for seven weeks; a warning per lane plus the
                // `failed` counter returned to the caller is the minimum that makes
                // the condition observable.
                //
                // Leaving the floor unraised is what keeps the entity repairable from
                // the sender's side: the cursor advances over this blob, but the
                // sender's next push of the same entity arrives under a new
                // `change_seq` and is accepted rather than discarded as a stale
                // replay. That alone leaves the entity waiting for an edit that may
                // never come, so on refusal the blob is ALSO queued locally and
                // replayed on later cycles (ADR-058). The merge borrows it, so the
                // bytes are still here to hand over, and nothing is copied on the
                // path where the merge succeeds.
                let change = InboundChange {
                    entity,
                    deleted: frame.d,
                    changeset: frame.c,
                };
                let outcome = match merge_lane(engine, &change).await {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        tracing::warn!(
                            entity_type = %change.entity.entity_type,
                            entity_uuid = %change.entity.entity_uuid,
                            stage = e.stage(),
                            error = %e.message(),
                            "could not merge an inbound lane; skipping it and leaving the \
                             anti-rollback floor unraised so a later push can still apply it"
                        );
                        stats.failed += 1;
                        if queued < MAX_PENDING_LANES {
                            let pending = PendingLane {
                                opaque_id: lane.opaque_id.clone(),
                                device_id: lane.device_id.clone(),
                                entity: change.entity,
                                deleted: change.deleted,
                                changeset: change.changeset,
                                hlc: frame.h,
                                attempts: 0,
                            };
                            // Best-effort, like every queue write: propagating this
                            // would abort the cycle before the pull cursor advances
                            // and freeze every other lane of the account, which is
                            // the failure ADR-056 removed. Worse, the causes that
                            // make this INSERT fail (a full disk, a locked database)
                            // are the very ones the queue exists to survive. On
                            // failure the lane simply falls back to waiting for its
                            // sender, which is where it was before ADR-058.
                            match state.put_pending_lane(&ctx.account_id, &pending).await {
                                Ok(()) => queued += 1,
                                Err(e) => tracing::warn!(
                                    entity_type = %pending.entity.entity_type,
                                    entity_uuid = %pending.entity.entity_uuid,
                                    error = %e,
                                    "could not queue a refused lane for retry; it waits for \
                                     its sender to push that entity again"
                                ),
                            }
                        } else {
                            // Never silently: a lane that is not queued is one this
                            // device only recovers if its sender pushes it again.
                            tracing::warn!(
                                entity_type = %change.entity.entity_type,
                                entity_uuid = %change.entity.entity_uuid,
                                cap = MAX_PENDING_LANES,
                                "the lane retry queue is full; this lane is not queued and \
                                 waits for its sender to push that entity again"
                            );
                        }
                        continue;
                    }
                };
                // A changeset that materialized a row without all its NOT NULL columns
                // came from a sender predating ADR-056. Keep the partial row (the
                // columns it does carry are correct) but leave the anti-rollback floor
                // where it is.
                //
                // This does NOT re-deliver the blob: the pull cursor advances over it
                // like any other. What the unraised floor buys is that the sender's
                // next push of that entity, which carries the whole row under a new
                // `change_seq`, is accepted instead of being discarded as a stale
                // replay. Raising the floor here would freeze the amputated row for
                // good, since nothing ever lowers it again.
                if !outcome.complete {
                    tracing::warn!(
                        entity_type = %change.entity.entity_type,
                        entity_uuid = %change.entity.entity_uuid,
                        device = %lane.device_id,
                        "applied an incomplete changeset: NOT NULL columns fell back to \
                         their defaults; the anti-rollback floor is left unraised so a \
                         complete changeset can still repair this row"
                    );
                    stats.applied += 1;
                    continue;
                }
            }
            // Raise the per-lane anti-rollback floor only after a successful apply.
            state
                .set_lane_hlc(&ctx.account_id, &lane.opaque_id, &lane.device_id, frame.h)
                .await?;
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

    let device_id_bytes = ctx.device_id.as_bytes();
    let mut lanes = Vec::with_capacity(changes.len());
    for change in changes {
        lanes.push(seal_lane(
            bundle,
            account_aad,
            device_id_bytes,
            change.entity.entity_type,
            change.entity.entity_uuid,
            change.deleted,
            change.changeset,
            change.hlc,
        )?);
    }

    // Custom cover bytes (ADR-046). cr-sqlite carries the cover_url row but not the
    // file, so the bytes ride their own "cover" lanes here. The source dedups
    // against its local `cover_sync_state`, so only covers that changed since the
    // last successful push are re-encoded and uploaded. We report the ones pushed
    // this cycle in `stats.pushed_covers`; the caller records them in the dedup
    // state AFTER the whole cycle succeeds. The source has already re-encoded each
    // returned cover to fit a lane blob.
    for cover in cover_source.pending_covers().await? {
        stats
            .pushed_covers
            .push((cover.book_uuid.clone(), cover.hlc));
        lanes.push(seal_lane(
            bundle,
            account_aad,
            device_id_bytes,
            COVER_ENTITY_TYPE.to_string(),
            cover.book_uuid,
            false,
            cover.bytes,
            cover.hlc,
        )?);
    }

    if !lanes.is_empty() {
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
/// stale registry. An enrolled device (one that has already adopted a signed
/// registry, `registry_seq > 0`) that gets `None` back from the refresh REFUSES
/// the cycle with [`SyncError::Registry`] instead of falling back to "accept all
/// lanes": otherwise a hostile or buggy hub could disable device authorization by
/// simply withholding the registry. The `None`-accepts-all fallback survives only
/// for a device that has never adopted a registry (true first bootstrap), where no
/// authorization baseline exists yet.
///
/// This is the seam the account FFI entrypoint
/// (`account_sync_now_ffi`) and the future periodic/on-resume triggers call;
/// the production merge `engine` is supplied by the caller (future work).
pub async fn refresh_then_sync(
    transport: &dyn LaneTransport,
    engine: &dyn MergeEngine,
    bundle: &AccountKeyBundle,
    state: &dyn SyncStateStore,
    account_id: &str,
    device_id: &str,
) -> std::result::Result<SyncStats, SyncError> {
    refresh_then_sync_with_covers(
        transport,
        engine,
        bundle,
        state,
        account_id,
        device_id,
        &NoopCovers {},
        &NoopCovers {},
    )
    .await
}

/// [`refresh_then_sync`] that also transports custom cover bytes through the given
/// cover seams (ADR-046). This is the entrypoint the account FFI uses on the real
/// build so a hand-photographed cover reaches the user's other devices.
#[allow(clippy::too_many_arguments)]
pub async fn refresh_then_sync_with_covers(
    transport: &dyn LaneTransport,
    engine: &dyn MergeEngine,
    bundle: &AccountKeyBundle,
    state: &dyn SyncStateStore,
    account_id: &str,
    device_id: &str,
    cover_source: &dyn CoverSource,
    cover_sink: &dyn CoverSink,
) -> std::result::Result<SyncStats, SyncError> {
    let authorized_devices =
        refresh_authorized_devices(transport, state, account_id, &bundle.verifying_key()).await?;
    // H3 hardening: once this device has adopted a signed registry it must never
    // sync against an absent one. A hub that withholds the registry would otherwise
    // re-enable the "accept all lanes" fallback and smuggle in revoked/unauthorized
    // lanes; refuse the cycle instead.
    if authorized_devices.is_none() && state.registry_seq(account_id).await? > 0 {
        return Err(SyncError::Registry(
            "hub served no device registry for an enrolled account".to_string(),
        ));
    }
    let ctx = SyncContext {
        account_id: account_id.to_string(),
        device_id: device_id.to_string(),
        authorized_devices,
    };
    sync_once_with_covers(
        transport,
        engine,
        bundle,
        state,
        &ctx,
        cover_source,
        cover_sink,
    )
    .await
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

/// Remove `device_id` from the account's signed registry: fetch the current registry,
/// adopt it (so we always shrink the latest signed version, never a stale one), drop the
/// device, bump the signed `registry_seq`, re-sign with the account key, and publish.
/// Persists the new seq and returns the updated registry.
///
/// After a peer adopts the republished registry, the removed device's `device_id` is no
/// longer authorized, so H3 filters its lanes and it can no longer write into the shared
/// library. This is a **soft** removal — the removed device keeps the trousseau and can
/// still read current content or re-sign itself back in; a hard lockout needs account key
/// rotation (deferred, ADR-042 section 13.5). Authorization ("who may remove") is enforced
/// by the caller, not here (the FFI refuses removing the current device). Returns
/// [`SyncError::Registry`] if the hub has no registry yet (nothing to remove from).
pub async fn remove_device(
    transport: &dyn LaneTransport,
    state: &dyn SyncStateStore,
    bundle: &AccountKeyBundle,
    account_id: &str,
    device_id: &str,
) -> std::result::Result<DeviceRegistry, SyncError> {
    let current = fetch_and_adopt(transport, state, account_id, &bundle.verifying_key())
        .await?
        .ok_or_else(|| SyncError::Registry("no registry to remove from".to_string()))?;

    let updated = current.without_device(device_id);
    let signed = updated
        .sign(&bundle.signing_key())
        .map_err(|e| SyncError::Crypto(e.to_string()))?;
    transport
        .publish_registry(&encode_blob_standard(&signed))
        .await?;
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
        //
        // A row created here is born under the ADR-056 engine, so it needs no
        // repair: `full_repush_done` is set on INSERT. Leaving it to the column
        // default (0, which is what backfills the rows that DO need repairing)
        // would make migration 092 reset the watermark on the next boot and
        // republish the whole library a second time after every enrolment. The
        // ON CONFLICT branch deliberately leaves the flag alone, so a row that
        // predates the migration keeps its pending repair.
        let sql = format!(
            "INSERT INTO account_sync_state (account_id, {col}, last_synced_at, full_repush_done) \
             VALUES (?, ?, datetime('now'), 1) \
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

    async fn lane_hlc(
        &self,
        account_id: &str,
        opaque_id: &str,
        device_id: &str,
    ) -> std::result::Result<i64, SyncError> {
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT last_hlc AS v FROM account_lane_hlc \
                 WHERE account_id = ? AND opaque_id = ? AND device_id = ?",
                [account_id.into(), opaque_id.into(), device_id.into()],
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

    async fn set_lane_hlc(
        &self,
        account_id: &str,
        opaque_id: &str,
        device_id: &str,
        hlc: i64,
    ) -> std::result::Result<(), SyncError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                // The floor is monotonic: take MAX so it can never regress, even if two
                // sync cycles for the same account interleave their reads and writes (the
                // single-threaded caller already only raises it; this hardens the invariant
                // at the storage layer for when concurrent triggers are wired).
                "INSERT INTO account_lane_hlc \
                 (account_id, opaque_id, device_id, last_hlc, updated_at) \
                 VALUES (?, ?, ?, ?, datetime('now')) \
                 ON CONFLICT(account_id, opaque_id, device_id) DO UPDATE SET \
                 last_hlc = MAX(account_lane_hlc.last_hlc, excluded.last_hlc), \
                 updated_at = datetime('now')",
                [
                    account_id.into(),
                    opaque_id.into(),
                    device_id.into(),
                    hlc.into(),
                ],
            ))
            .await
            .map_err(|e| SyncError::State(e.to_string()))?;
        Ok(())
    }

    async fn pending_lanes(
        &self,
        account_id: &str,
    ) -> std::result::Result<Vec<(PendingLane, i64)>, SyncError> {
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                // Oldest first: an upsert keeps the row's rowid, so a lane that has
                // been waiting the longest is retried first, cycle after cycle.
                //
                // The anti-rollback floor is joined in rather than read per entry:
                // a saturated queue would otherwise cost one extra query per lane
                // on every cycle. LEFT JOIN because a lane whose blob has never been
                // applied has no floor row at all, which reads as 0.
                "SELECT p.opaque_id, p.device_id, p.entity_type, p.entity_uuid, \
                 p.deleted, p.changeset, p.hlc, p.attempts, \
                 COALESCE(h.last_hlc, 0) AS floor \
                 FROM account_pending_lane p \
                 LEFT JOIN account_lane_hlc h \
                 ON h.account_id = p.account_id AND h.opaque_id = p.opaque_id \
                 AND h.device_id = p.device_id \
                 WHERE p.account_id = ? ORDER BY p.rowid",
                [account_id.into()],
            ))
            .await
            .map_err(|e| SyncError::State(e.to_string()))?;
        rows.into_iter()
            .map(
                |r| -> std::result::Result<(PendingLane, i64), sea_orm::DbErr> {
                    Ok((
                        PendingLane {
                            opaque_id: r.try_get("", "opaque_id")?,
                            device_id: r.try_get("", "device_id")?,
                            entity: EntityRef {
                                entity_type: r.try_get("", "entity_type")?,
                                entity_uuid: r.try_get("", "entity_uuid")?,
                            },
                            deleted: r.try_get::<i64>("", "deleted")? != 0,
                            changeset: r.try_get("", "changeset")?,
                            hlc: r.try_get("", "hlc")?,
                            attempts: r.try_get("", "attempts")?,
                        },
                        r.try_get("", "floor")?,
                    ))
                },
            )
            .collect::<std::result::Result<Vec<_>, sea_orm::DbErr>>()
            .map_err(|e| SyncError::State(e.to_string()))
    }

    async fn put_pending_lane(
        &self,
        account_id: &str,
        lane: &PendingLane,
    ) -> std::result::Result<(), SyncError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                // One row per lane: a newer blob replaces the older one exactly as it
                // does in the hub's snapshot store, so the queue can never accumulate
                // several generations of the same entity. `first_seen_at` survives the
                // replacement, so how long a lane has been stuck stays readable.
                "INSERT INTO account_pending_lane \
                 (account_id, opaque_id, device_id, entity_type, entity_uuid, deleted, \
                 changeset, hlc, attempts, first_seen_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now')) \
                 ON CONFLICT(account_id, opaque_id, device_id) DO UPDATE SET \
                 entity_type = excluded.entity_type, entity_uuid = excluded.entity_uuid, \
                 deleted = excluded.deleted, changeset = excluded.changeset, \
                 hlc = excluded.hlc, attempts = excluded.attempts, \
                 updated_at = datetime('now')",
                [
                    account_id.into(),
                    lane.opaque_id.clone().into(),
                    lane.device_id.clone().into(),
                    lane.entity.entity_type.clone().into(),
                    lane.entity.entity_uuid.clone().into(),
                    i64::from(lane.deleted).into(),
                    lane.changeset.clone().into(),
                    lane.hlc.into(),
                    lane.attempts.into(),
                ],
            ))
            .await
            .map_err(|e| SyncError::State(e.to_string()))?;
        Ok(())
    }

    async fn drop_pending_lane(
        &self,
        account_id: &str,
        opaque_id: &str,
        device_id: &str,
    ) -> std::result::Result<(), SyncError> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "DELETE FROM account_pending_lane \
                 WHERE account_id = ? AND opaque_id = ? AND device_id = ?",
                [account_id.into(), opaque_id.into(), device_id.into()],
            ))
            .await
            .map_err(|e| SyncError::State(e.to_string()))?;
        Ok(())
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
        // key: (account_id, opaque_id, device_id) -> last applied HLC.
        lane: Mutex<HashMap<(String, String, String), i64>>,
        // Retry queue (ADR-058), insertion-ordered like the SQLite one.
        pending: Mutex<Vec<(String, PendingLane)>>,
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
        async fn lane_hlc(
            &self,
            account_id: &str,
            opaque_id: &str,
            device_id: &str,
        ) -> std::result::Result<i64, SyncError> {
            let key = (
                account_id.to_string(),
                opaque_id.to_string(),
                device_id.to_string(),
            );
            Ok(*self.lane.lock().unwrap().get(&key).unwrap_or(&0))
        }
        async fn set_lane_hlc(
            &self,
            account_id: &str,
            opaque_id: &str,
            device_id: &str,
            hlc: i64,
        ) -> std::result::Result<(), SyncError> {
            let key = (
                account_id.to_string(),
                opaque_id.to_string(),
                device_id.to_string(),
            );
            self.lane.lock().unwrap().insert(key, hlc);
            Ok(())
        }

        async fn pending_lanes(
            &self,
            account_id: &str,
        ) -> std::result::Result<Vec<(PendingLane, i64)>, SyncError> {
            let floors = self.lane.lock().unwrap();
            Ok(self
                .pending
                .lock()
                .unwrap()
                .iter()
                .filter(|(acct, _)| acct == account_id)
                .map(|(_, lane)| {
                    let key = (
                        account_id.to_string(),
                        lane.opaque_id.clone(),
                        lane.device_id.clone(),
                    );
                    (lane.clone(), *floors.get(&key).unwrap_or(&0))
                })
                .collect())
        }

        async fn put_pending_lane(
            &self,
            account_id: &str,
            lane: &PendingLane,
        ) -> std::result::Result<(), SyncError> {
            let mut queue = self.pending.lock().unwrap();
            match queue.iter_mut().find(|(acct, l)| {
                acct == account_id && l.opaque_id == lane.opaque_id && l.device_id == lane.device_id
            }) {
                Some((_, existing)) => *existing = lane.clone(),
                None => queue.push((account_id.to_string(), lane.clone())),
            }
            Ok(())
        }

        async fn drop_pending_lane(
            &self,
            account_id: &str,
            opaque_id: &str,
            device_id: &str,
        ) -> std::result::Result<(), SyncError> {
            self.pending.lock().unwrap().retain(|(acct, l)| {
                acct != account_id || l.opaque_id != opaque_id || l.device_id != device_id
            });
            Ok(())
        }
    }

    // --- in-memory stateful hub (mirrors the ADR-043 lane semantics) ---

    #[derive(Default, Clone)]
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
        // Records every (entity_type, entity_uuid) sync_once passed to
        // repair_after_apply, so a test can assert the hook fires per applied change.
        repaired: Mutex<Vec<(String, String)>>,
    }

    impl FakeEngine {
        fn new(device: &str) -> Self {
            Self {
                device: device.to_string(),
                clock: Mutex::new(0),
                version: Mutex::new(0),
                store: Mutex::new(HashMap::new()),
                repaired: Mutex::new(Vec::new()),
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
                        // The HLC counter is monotonic per device, so each re-edit of an
                        // entity pushes a strictly higher lane HLC (anti-rollback).
                        hlc: rec.hlc.0,
                    });
                }
            }
            Ok(out)
        }

        async fn apply(
            &self,
            change: &InboundChange,
        ) -> std::result::Result<ApplyOutcome, MergeEngineError> {
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
            // The fake stores a whole value, so a partial row cannot arise here.
            Ok(ApplyOutcome::complete())
        }

        async fn repair_after_apply(
            &self,
            entity_type: &str,
            entity_uuid: &str,
        ) -> std::result::Result<(), MergeEngineError> {
            self.repaired
                .lock()
                .unwrap()
                .push((entity_type.to_string(), entity_uuid.to_string()));
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

    /// A cover source/sink that hands out a preset list and records what it is
    /// asked to write. Lets one test push covers from device A and capture what
    /// device B persists (ADR-046).
    #[derive(Default)]
    struct CollectingCovers {
        to_push: Mutex<Vec<OutboundCover>>,
        written: Mutex<Vec<(String, Vec<u8>, i64)>>,
    }

    #[async_trait]
    impl CoverSource for CollectingCovers {
        async fn pending_covers(&self) -> std::result::Result<Vec<OutboundCover>, SyncError> {
            Ok(self.to_push.lock().unwrap().clone())
        }
    }

    #[async_trait]
    impl CoverSink for CollectingCovers {
        async fn write_cover(
            &self,
            book_uuid: &str,
            bytes: &[u8],
            hlc: i64,
        ) -> std::result::Result<(), SyncError> {
            self.written
                .lock()
                .unwrap()
                .push((book_uuid.to_string(), bytes.to_vec(), hlc));
            Ok(())
        }
    }

    /// A [`FakeEngine`] wrapper that refuses to merge (or to repair after merging)
    /// what it is told to. Stands in for any engine-level refusal; the error text is
    /// the one a real device produced when its cr-sqlite connection had been
    /// released out from under the merge (ADR-056).
    ///
    /// The refusals live behind a `Mutex` so a test can lift them mid-run
    /// ([`RejectingEngine::heal`]): a refusal that clears is what tells a transient
    /// failure apart from a definitive one (ADR-058), and the retry queue exists
    /// precisely for the first.
    struct RejectingEngine {
        inner: FakeEngine,
        /// Refuse to apply this entity uuid.
        reject_apply: Mutex<Option<String>>,
        /// Refuse the post-merge repair of this entity uuid.
        reject_repair: Mutex<Option<String>>,
        /// Refuse to apply any changeset carrying this value, whatever its entity.
        /// Lets a test refuse one GENERATION of an entity and accept the next.
        reject_value: Mutex<Option<String>>,
        /// Refuse everything, for the queue-cap test.
        reject_all: Mutex<bool>,
    }

    impl RejectingEngine {
        /// A wrapper that refuses nothing yet.
        fn wrapping(device: &str) -> Self {
            Self {
                inner: FakeEngine::new(device),
                reject_apply: Mutex::new(None),
                reject_repair: Mutex::new(None),
                reject_value: Mutex::new(None),
                reject_all: Mutex::new(false),
            }
        }

        fn rejecting_apply(device: &str, uuid: &str) -> Self {
            let engine = Self::wrapping(device);
            *engine.reject_apply.lock().unwrap() = Some(uuid.to_string());
            engine
        }

        fn rejecting_repair(device: &str, uuid: &str) -> Self {
            let engine = Self::wrapping(device);
            *engine.reject_repair.lock().unwrap() = Some(uuid.to_string());
            engine
        }

        fn rejecting_value(device: &str, value: &str) -> Self {
            let engine = Self::wrapping(device);
            *engine.reject_value.lock().unwrap() = Some(value.to_string());
            engine
        }

        fn rejecting_everything(device: &str) -> Self {
            let engine = Self::wrapping(device);
            *engine.reject_all.lock().unwrap() = true;
            engine
        }

        /// Lift every refusal: the transient cause cleared.
        fn heal(&self) {
            *self.reject_apply.lock().unwrap() = None;
            *self.reject_repair.lock().unwrap() = None;
            *self.reject_value.lock().unwrap() = None;
            *self.reject_all.lock().unwrap() = false;
        }
    }

    #[async_trait]
    impl MergeEngine for RejectingEngine {
        async fn local_version(&self) -> std::result::Result<i64, MergeEngineError> {
            self.inner.local_version().await
        }

        async fn changes_since(
            &self,
            since: i64,
        ) -> std::result::Result<Vec<OutboundChange>, MergeEngineError> {
            self.inner.changes_since(since).await
        }

        async fn apply(
            &self,
            change: &InboundChange,
        ) -> std::result::Result<ApplyOutcome, MergeEngineError> {
            let value_refused =
                self.reject_value
                    .lock()
                    .unwrap()
                    .as_deref()
                    .is_some_and(|refused| {
                        rmp_serde::from_slice::<FakeChangeset>(&change.changeset)
                            .is_ok_and(|cs| cs.value == refused)
                    });
            if *self.reject_all.lock().unwrap()
                || value_refused
                || self.reject_apply.lock().unwrap().as_deref()
                    == Some(change.entity.entity_uuid.as_str())
            {
                return Err(MergeEngineError(
                    "error returned from database: (code: 1) Failed to update CRR table information"
                        .to_string(),
                ));
            }
            self.inner.apply(change).await
        }

        async fn repair_after_apply(
            &self,
            entity_type: &str,
            entity_uuid: &str,
        ) -> std::result::Result<(), MergeEngineError> {
            if self.reject_repair.lock().unwrap().as_deref() == Some(entity_uuid) {
                return Err(MergeEngineError("cascade failed".to_string()));
            }
            self.inner
                .repair_after_apply(entity_type, entity_uuid)
                .await
        }
    }

    /// A sink whose every write fails, to exercise cover-write error isolation.
    struct FailingCoverSink;

    #[async_trait]
    impl CoverSink for FailingCoverSink {
        async fn write_cover(
            &self,
            _book_uuid: &str,
            _bytes: &[u8],
            _hlc: i64,
        ) -> std::result::Result<(), SyncError> {
            Err(SyncError::State("disk full".to_string()))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cover_pushed_by_one_device_is_written_by_another() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = FakeEngine::new("devA");
        let eng_b = FakeEngine::new("devB");
        let state_a = MemState::default();
        let state_b = MemState::default();

        let a_covers = CollectingCovers::default();
        a_covers.to_push.lock().unwrap().push(OutboundCover {
            book_uuid: "book-1".to_string(),
            bytes: b"the-jpeg-bytes".to_vec(),
            hlc: 100,
        });
        let b_covers = CollectingCovers::default();

        // A pushes its cover lane (no row edits needed for this test).
        let a_stats = sync_once_with_covers(
            &*hub,
            &eng_a,
            &bundle,
            &state_a,
            &ctx("devA"),
            &a_covers,
            &NoopCovers {},
        )
        .await
        .unwrap();
        // The pushed cover is reported back so the caller can record it in the
        // local dedup state (ADR-046): it carries the book uuid + file mtime.
        assert_eq!(a_stats.pushed_covers, vec![("book-1".to_string(), 100)]);

        // B pulls: the cover lane goes to B's sink, never into B's merge engine.
        sync_once_with_covers(
            &*hub,
            &eng_b,
            &bundle,
            &state_b,
            &ctx("devB"),
            &NoopCovers {},
            &b_covers,
        )
        .await
        .unwrap();

        // Scope the guard so it is released before the next await below
        // (clippy `await_holding_lock` does not credit an explicit `drop`).
        {
            let written = b_covers.written.lock().unwrap();
            assert_eq!(written.len(), 1, "B should have written exactly one cover");
            assert_eq!(written[0].0, "book-1");
            assert_eq!(written[0].1, b"the-jpeg-bytes");
            assert_eq!(written[0].2, 100);
        }
        // The cover did not leak into the row-merge engine.
        assert!(
            eng_b.snapshot().is_empty(),
            "cover bytes must not become a merged entity row"
        );

        // A second sync is idempotent: the pull cursor has advanced, so B does not
        // rewrite the cover.
        sync_once_with_covers(
            &*hub,
            &eng_b,
            &bundle,
            &state_b,
            &ctx("devB"),
            &NoopCovers {},
            &b_covers,
        )
        .await
        .unwrap();
        assert_eq!(b_covers.written.lock().unwrap().len(), 1, "no rewrite");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_cover_write_is_isolated_and_does_not_block_row_sync() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = FakeEngine::new("devA");
        let eng_b = FakeEngine::new("devB");
        let state_a = MemState::default();
        let state_b = MemState::default();

        // A pushes both a row edit and a custom cover.
        eng_a.edit("book-1", "v1", false);
        let a_covers = CollectingCovers::default();
        a_covers.to_push.lock().unwrap().push(OutboundCover {
            book_uuid: "book-9".to_string(),
            bytes: b"jpeg".to_vec(),
            hlc: 5,
        });
        sync_once_with_covers(
            &*hub,
            &eng_a,
            &bundle,
            &state_a,
            &ctx("devA"),
            &a_covers,
            &NoopCovers {},
        )
        .await
        .unwrap();

        // B pulls with a sink that always fails: the cycle still succeeds and the
        // row still applies.
        sync_once_with_covers(
            &*hub,
            &eng_b,
            &bundle,
            &state_b,
            &ctx("devB"),
            &NoopCovers {},
            &FailingCoverSink,
        )
        .await
        .expect("a failing cover write must not abort the sync cycle");
        assert_eq!(
            eng_b.snapshot(),
            vec![("book-1".to_string(), "v1".to_string(), false)],
            "row sync must complete despite the failing cover write"
        );

        // The cover's HLC floor was NOT raised, so a later working sink retries it.
        // (Reset B's cursor to re-pull the same lanes; the already-applied row lane
        // is skipped by its own floor, only the never-applied cover is retried.)
        state_b.set_pull_cursor("acct-1", 0).await.unwrap();
        let good = CollectingCovers::default();
        sync_once_with_covers(
            &*hub,
            &eng_b,
            &bundle,
            &state_b,
            &ctx("devB"),
            &NoopCovers {},
            &good,
        )
        .await
        .unwrap();
        let written = good.written.lock().unwrap();
        assert_eq!(
            written.len(),
            1,
            "cover retried after the floor stayed unraised"
        );
        assert_eq!(written[0].0, "book-9");
    }

    /// Push three books from A in three separate cycles, so the hub hands them to B
    /// in a deterministic `change_seq` order (`book-1`, `book-2`, `book-3`).
    async fn hub_with_three_books(
        hub: &MemHub,
        bundle: &AccountKeyBundle,
    ) -> std::result::Result<(), SyncError> {
        let eng_a = FakeEngine::new("devA");
        let state_a = MemState::default();
        for uuid in ["book-1", "book-2", "book-3"] {
            eng_a.edit(uuid, "v1", false);
            sync_once(hub, &eng_a, bundle, &state_a, &ctx("devA")).await?;
        }
        Ok(())
    }

    fn lane_id(bundle: &AccountKeyBundle, uuid: &str) -> String {
        encode_b64url(&bundle.opaque_id("book", uuid))
    }

    // A lane this device cannot merge must NOT abort the cycle. Before ADR-056's
    // lane isolation the apply error propagated with `?`, so the pull cursor never
    // advanced and one unappliable entity froze every other lane of the account
    // permanently, which is exactly what a mixed-version fleet produces and what
    // made the ADR-056 rollout unreleasable.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unappliable_lane_is_skipped_and_the_rest_of_the_cycle_converges() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        hub_with_three_books(&hub, &bundle).await.unwrap();

        let eng_b = RejectingEngine::rejecting_apply("devB", "book-2");
        let state_b = MemState::default();
        let stats = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .expect("one unappliable lane must not abort the cycle");

        assert_eq!(stats.failed, 1, "the skip is counted, not swallowed");
        assert_eq!(stats.applied, 2, "the two healthy lanes still applied");
        assert_eq!(
            eng_b.inner.snapshot(),
            vec![
                ("book-1".to_string(), "v1".to_string(), false),
                ("book-3".to_string(), "v1".to_string(), false),
            ],
            "the lanes after the failing one must still be merged"
        );

        // The cursor advanced past all three, so the cycle is not stuck: without the
        // isolation it would sit at 0 forever.
        assert_eq!(state_b.pull_cursor("acct-1").await.unwrap(), 3);

        // The anti-rollback floor was raised for what applied and left alone for what
        // did not, which is what keeps the skipped entity repairable.
        assert!(
            state_b
                .lane_hlc("acct-1", &lane_id(&bundle, "book-1"), "devA")
                .await
                .unwrap()
                > 0
        );
        assert_eq!(
            state_b
                .lane_hlc("acct-1", &lane_id(&bundle, "book-2"), "devA")
                .await
                .unwrap(),
            0,
            "the floor of a skipped lane must stay unraised"
        );
    }

    // Skipping without raising the floor is only worth anything if the entity can
    // still heal. Re-deliver the very same blobs to a device whose engine now
    // accepts them: the skipped one applies, the two already-applied ones are
    // rejected by their own floors.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_skipped_lane_still_applies_once_the_receiver_can_merge_it() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        hub_with_three_books(&hub, &bundle).await.unwrap();

        let state_b = MemState::default();
        let broken = RejectingEngine::rejecting_apply("devB", "book-2");
        sync_once(&*hub, &broken, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();

        // Same lanes, same HLCs, a receiver that can merge them now.
        state_b.set_pull_cursor("acct-1", 0).await.unwrap();
        let healthy = FakeEngine::new("devB");
        let stats = sync_once(&*hub, &healthy, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();

        assert_eq!(stats.failed, 0);
        assert_eq!(
            stats.applied, 1,
            "only the lane whose floor stayed unraised is re-applied"
        );
        assert_eq!(
            healthy.snapshot(),
            vec![("book-2".to_string(), "v1".to_string(), false)],
            "the previously skipped entity heals through the ordinary path"
        );
    }

    // The post-merge referential repair is isolated on the same terms: the merge
    // landed, but the entity is left in a state this cycle could not finish, so the
    // lane is counted, logged, and left repairable rather than aborting everything.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_post_merge_repair_is_isolated_too() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        hub_with_three_books(&hub, &bundle).await.unwrap();

        let eng_b = RejectingEngine::rejecting_repair("devB", "book-2");
        let state_b = MemState::default();
        let stats = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .expect("a failing integrity repair must not abort the cycle");

        assert_eq!(stats.failed, 1);
        assert_eq!(stats.applied, 2);
        assert_eq!(state_b.pull_cursor("acct-1").await.unwrap(), 3);
        assert_eq!(
            state_b
                .lane_hlc("acct-1", &lane_id(&bundle, "book-2"), "devA")
                .await
                .unwrap(),
            0,
            "an unrepaired lane must stay repairable too"
        );
    }

    // The point of ADR-058. A lane skipped for a TRANSIENT reason must heal on its
    // own: nothing else re-delivers it (the pull cursor advanced over it and the
    // sender's push watermark advanced too), so before the retry queue this entity
    // stayed behind until someone happened to edit it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lane_that_failed_transiently_is_retried_and_heals_with_no_new_push() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        hub_with_three_books(&hub, &bundle).await.unwrap();

        let eng_b = RejectingEngine::rejecting_apply("devB", "book-2");
        let state_b = MemState::default();
        let first = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        assert_eq!(first.failed, 1);
        assert_eq!(state_b.pending_lanes("acct-1").await.unwrap().len(), 1);

        // The transient cause clears. The hub has nothing new: no push from devA,
        // and the cursor already sits past all three lanes.
        eng_b.heal();
        let second = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();

        assert_eq!(second.failed, 0);
        assert_eq!(second.applied, 1, "the queued lane merged on retry");
        assert_eq!(
            eng_b.inner.snapshot(),
            vec![
                ("book-1".to_string(), "v1".to_string(), false),
                ("book-2".to_string(), "v1".to_string(), false),
                ("book-3".to_string(), "v1".to_string(), false),
            ],
            "the skipped entity healed without its sender pushing it again"
        );
        assert!(
            state_b
                .lane_hlc("acct-1", &lane_id(&bundle, "book-2"), "devA")
                .await
                .unwrap()
                > 0,
            "a successful retry raises the anti-rollback floor it had left alone"
        );
        assert!(
            state_b.pending_lanes("acct-1").await.unwrap().is_empty(),
            "a merged lane leaves the queue"
        );
    }

    // H5 (ADR-042 §14) applied to the queue. A queued blob is replayed only while
    // it is still ahead of its lane's floor: once a fresher blob for the same lane
    // has merged, replaying the old one is exactly the rollback the floor exists to
    // prevent, so the entry is dropped unread.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_lane_overtaken_by_a_fresher_blob_is_dropped_not_replayed() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = FakeEngine::new("devA");
        let state_a = MemState::default();
        eng_a.edit("book-1", "v1", false);
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();

        // B refuses this generation of the entity, so "v1" lands in the queue.
        let eng_b = RejectingEngine::rejecting_value("devB", "v1");
        let state_b = MemState::default();
        sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        let queued = state_b.pending_lanes("acct-1").await.unwrap();
        assert_eq!(queued.len(), 1);
        let stale_hlc = queued[0].0.hlc;

        // A edits the same book. The lane's blob is replaced on the hub, and B can
        // merge this generation: the retry of "v1" fails again first, then "v2"
        // applies and raises the floor above the queued blob.
        eng_a.edit("book-1", "v2", false);
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();
        sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        let floor = state_b
            .lane_hlc("acct-1", &lane_id(&bundle, "book-1"), "devA")
            .await
            .unwrap();
        assert!(
            floor > stale_hlc,
            "the fresher blob must have raised the floor above the queued one"
        );

        // The next cycle drains the queue. The stale entry must be discarded, not
        // merged: merging it would roll book-1 back from "v2" to "v1".
        let third = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        assert_eq!(third.failed, 0, "a superseded entry is not a failure");
        assert_eq!(third.applied, 0, "and it is not applied either");
        assert_eq!(
            eng_b.inner.snapshot(),
            vec![("book-1".to_string(), "v2".to_string(), false)],
            "the entity must not be rolled back by its own retry queue"
        );
        assert!(
            state_b.pending_lanes("acct-1").await.unwrap().is_empty(),
            "the superseded entry is dropped rather than retried forever"
        );
    }

    // Retrying is bounded. A refusal that survives every attempt is definitive in
    // practice, and the queue gives up on it rather than replaying it for the life
    // of the install. Giving up restores the ADR-056 state exactly: floor unraised,
    // entity waiting for its sender.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lane_that_never_merges_is_given_up_after_bounded_retries() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        hub_with_three_books(&hub, &bundle).await.unwrap();

        let eng_b = RejectingEngine::rejecting_apply("devB", "book-2");
        let state_b = MemState::default();
        // Cycle 1 pulls and queues it; each later cycle spends one attempt, so the
        // budget runs out on the cycle after the last one.
        for _ in 0..=MAX_PENDING_LANE_ATTEMPTS {
            let stats = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
                .await
                .unwrap();
            assert_eq!(stats.failed, 1, "still counted while it is still failing");
        }

        assert!(
            state_b.pending_lanes("acct-1").await.unwrap().is_empty(),
            "the queue must not keep a lane past its attempt budget"
        );
        let after = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        assert_eq!(after.failed, 0, "nothing is retried once it is given up");
        assert_eq!(
            state_b
                .lane_hlc("acct-1", &lane_id(&bundle, "book-2"), "devA")
                .await
                .unwrap(),
            0,
            "giving up still leaves the floor unraised, so the sender can repair it"
        );
    }

    // The queue is storage on a device the performance policy targets, so it is
    // capped. A receiver that refuses everything (a wedged cr-sqlite connection
    // during a bootstrap pull) must not turn every inbound lane into a stored row.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_retry_queue_does_not_grow_past_its_cap() {
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = FakeEngine::new("devA");
        let state_a = MemState::default();
        let overflow = MAX_PENDING_LANES + 10;
        for i in 0..overflow {
            eng_a.edit(&format!("book-{i}"), "v1", false);
        }
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();

        let eng_b = RejectingEngine::rejecting_everything("devB");
        let state_b = MemState::default();
        let stats = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();

        assert_eq!(stats.failed, overflow, "every lane is still reported");
        assert_eq!(
            state_b.pending_lanes("acct-1").await.unwrap().len(),
            MAX_PENDING_LANES,
            "the queue stops at its cap; the rest wait for their sender as before"
        );
    }

    #[test]
    fn a_capped_cover_seals_within_the_hub_blob_limit() {
        // The hub rejects a lane blob whose base64 length exceeds 64 KiB
        // (AccountSyncController::MAX_BLOB_SIZE). A cover re-encoded to
        // COVER_SYNC_CAP_BYTES, framed and sealed, must stay under it — this is the
        // end-to-end budget that COVER_SYNC_CAP_BYTES is derived from (ADR-046).
        const MAX_BLOB_SIZE: usize = 64 * 1024;
        let bundle = AccountKeyBundle::generate();
        let bytes = vec![0xABu8; crate::utils::cover_image::COVER_SYNC_CAP_BYTES];
        let lane = seal_lane(
            &bundle,
            b"acct-1",
            b"a-full-length-device-id-base64url-padding-padding",
            COVER_ENTITY_TYPE.to_string(),
            "0190f5a2-1234-7abc-8def-0123456789ab".to_string(),
            false,
            bytes,
            1_900_000_000,
        )
        .unwrap();
        let b64_len = lane.blob.as_ref().unwrap().len();
        assert!(
            b64_len <= MAX_BLOB_SIZE,
            "sealed cover base64 is {b64_len} bytes, over the hub cap {MAX_BLOB_SIZE}"
        );
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
    async fn sync_once_repairs_referential_integrity_for_each_applied_change() {
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

        let stats = sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();

        // Every applied change runs the integrity-repair hook, with the entity it
        // applied. (A never repaired anything: it only pushed.)
        assert_eq!(stats.applied, 1);
        assert_eq!(
            *eng_b.repaired.lock().unwrap(),
            vec![("book".to_string(), "book-1".to_string())]
        );
        assert!(eng_a.repaired.lock().unwrap().is_empty());
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
    async fn h5_rejects_a_replayed_older_blob() {
        // A hostile hub re-serves an old-but-valid blob of a lane after a newer one
        // was already applied. The blob still decrypts (the AEAD binds it to its lane,
        // not to a sequence), so only the per-lane HLC floor can reject it.
        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = FakeEngine::new("devA");
        let eng_b = FakeEngine::new("devB");
        let state_a = MemState::default();
        let state_b = MemState::default();

        // devB publishes v1 of book-1, then devA applies it.
        eng_b.edit("book-1", "v1", false);
        sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        // Capture the v1 lane exactly as the hub stored it (the stale blob to replay).
        let (lane_key, stale_v1) = {
            let store = hub.lanes.lock().unwrap();
            let (k, l) = store.iter().next().expect("v1 lane pushed");
            (k.clone(), l.clone())
        };
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();
        assert_eq!(
            eng_a
                .snapshot()
                .iter()
                .find(|(u, ..)| u == "book-1")
                .unwrap()
                .1,
            "v1"
        );

        // devB publishes v2 (higher HLC, overwrites the lane); devA applies it.
        eng_b.edit("book-1", "v2", false);
        sync_once(&*hub, &eng_b, &bundle, &state_b, &ctx("devB"))
            .await
            .unwrap();
        sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();
        assert_eq!(
            eng_a
                .snapshot()
                .iter()
                .find(|(u, ..)| u == "book-1")
                .unwrap()
                .1,
            "v2"
        );

        // Hostile hub: replay the captured v1 blob under a fresh change_seq so devA's
        // monotonic pull cursor does not filter it (this is exactly the gap H5 closes).
        {
            let mut store = hub.lanes.lock().unwrap();
            let mut seq = hub.seq.lock().unwrap();
            *seq += 1;
            store.insert(
                lane_key,
                HubLane {
                    change_seq: *seq,
                    ..stale_v1
                },
            );
        }

        // devA syncs again: the replayed blob decrypts but its HLC has regressed, so
        // it is rejected. book-1 must NOT roll back to v1.
        let stats = sync_once(&*hub, &eng_a, &bundle, &state_a, &ctx("devA"))
            .await
            .unwrap();
        assert_eq!(stats.applied, 0, "stale replay must be rejected (H5)");
        assert_eq!(
            eng_a
                .snapshot()
                .iter()
                .find(|(u, ..)| u == "book-1")
                .unwrap()
                .1,
            "v2",
            "entity must not roll back to the replayed older blob"
        );
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
            // Mirrors `db::run_migrations`; keep both in step, `full_repush_done`
            // included (migration 092).
            "CREATE TABLE account_sync_state (account_id TEXT PRIMARY KEY, \
             pull_cursor INTEGER NOT NULL DEFAULT 0, \
             push_version INTEGER NOT NULL DEFAULT 0, \
             registry_seq INTEGER NOT NULL DEFAULT 0, last_synced_at TEXT, \
             full_repush_done INTEGER NOT NULL DEFAULT 0)"
                .to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE account_lane_hlc (account_id TEXT NOT NULL, \
             opaque_id TEXT NOT NULL, device_id TEXT NOT NULL, \
             last_hlc INTEGER NOT NULL DEFAULT 0, updated_at TEXT, \
             PRIMARY KEY (account_id, opaque_id, device_id))"
                .to_owned(),
        ))
        .await
        .unwrap();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            // Migration 093 (ADR-058).
            "CREATE TABLE account_pending_lane (account_id TEXT NOT NULL, \
             opaque_id TEXT NOT NULL, device_id TEXT NOT NULL, \
             entity_type TEXT NOT NULL, entity_uuid TEXT NOT NULL, \
             deleted INTEGER NOT NULL DEFAULT 0, changeset BLOB NOT NULL, \
             hlc INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, \
             first_seen_at TEXT, updated_at TEXT, \
             PRIMARY KEY (account_id, opaque_id, device_id))"
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

        // Per-lane HLC store (H5): unknown lane defaults to 0, then roundtrips,
        // and distinct lanes / accounts stay isolated.
        assert_eq!(store.lane_hlc("acct-1", "oid-1", "devB").await.unwrap(), 0);
        store
            .set_lane_hlc("acct-1", "oid-1", "devB", 42)
            .await
            .unwrap();
        assert_eq!(store.lane_hlc("acct-1", "oid-1", "devB").await.unwrap(), 42);
        // Upsert raises the same lane.
        store
            .set_lane_hlc("acct-1", "oid-1", "devB", 99)
            .await
            .unwrap();
        assert_eq!(store.lane_hlc("acct-1", "oid-1", "devB").await.unwrap(), 99);
        // The floor is monotonic: writing a lower value must NOT lower it (MAX).
        store
            .set_lane_hlc("acct-1", "oid-1", "devB", 50)
            .await
            .unwrap();
        assert_eq!(store.lane_hlc("acct-1", "oid-1", "devB").await.unwrap(), 99);
        // A different device on the same opaque_id is a distinct lane.
        assert_eq!(store.lane_hlc("acct-1", "oid-1", "devC").await.unwrap(), 0);
        // A different account is isolated.
        assert_eq!(store.lane_hlc("acct-2", "oid-1", "devB").await.unwrap(), 0);

        // Retry queue (ADR-058): the changeset must survive the BLOB round trip
        // intact, since replaying a truncated one would merge a partial row.
        assert!(store.pending_lanes("acct-1").await.unwrap().is_empty());
        let lane = PendingLane {
            opaque_id: "oid-1".to_string(),
            device_id: "devB".to_string(),
            entity: EntityRef {
                entity_type: "book".to_string(),
                entity_uuid: "book-1".to_string(),
            },
            deleted: false,
            changeset: vec![0x00, 0xFF, 0x10, 0x00, 0x7F],
            hlc: 42,
            attempts: 0,
        };
        store.put_pending_lane("acct-1", &lane).await.unwrap();
        // The floor rides along, joined from `account_lane_hlc`: this lane's is the
        // 99 set above, and a lane with no floor row at all reads as 0 (below).
        assert_eq!(
            store.pending_lanes("acct-1").await.unwrap(),
            vec![(lane.clone(), 99)]
        );
        // Distinct accounts are isolated here too.
        assert!(store.pending_lanes("acct-2").await.unwrap().is_empty());

        // Upsert on the same lane replaces the blob rather than queueing a second
        // generation of the same entity (one row per lane, like the hub's store).
        let refreshed = PendingLane {
            changeset: vec![0x01, 0x02],
            hlc: 43,
            attempts: 3,
            ..lane.clone()
        };
        store.put_pending_lane("acct-1", &refreshed).await.unwrap();
        assert_eq!(
            store.pending_lanes("acct-1").await.unwrap(),
            vec![(refreshed, 99)],
            "the newest blob supersedes the queued one"
        );

        // A different device on the same opaque_id is a different lane.
        let other_device = PendingLane {
            device_id: "devC".to_string(),
            ..lane.clone()
        };
        store
            .put_pending_lane("acct-1", &other_device)
            .await
            .unwrap();
        assert_eq!(store.pending_lanes("acct-1").await.unwrap().len(), 2);

        store
            .drop_pending_lane("acct-1", "oid-1", "devB")
            .await
            .unwrap();
        assert_eq!(
            store.pending_lanes("acct-1").await.unwrap(),
            vec![(other_device, 0)],
            "dropping one lane leaves the others queued, and a lane that has never \
             been applied joins no floor row"
        );
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
    async fn remove_republishes_signed_registry_without_the_device() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state = MemState::default();
        seed_registry(&hub, &registry_for(&["devA", "devB"]), &bundle).await; // seq 1

        let updated = remove_device(&hub, &state, &bundle, "acct-1", "devB")
            .await
            .unwrap();
        assert_eq!(updated.registry_seq, 2);
        assert!(updated.is_authorized("devA"));
        assert!(!updated.is_authorized("devB"));
        assert_eq!(state.registry_seq("acct-1").await.unwrap(), 2);

        // The republished blob on the hub verifies and no longer carries devB.
        let resp = hub.fetch_registry().await.unwrap();
        let blob = decode_blob_standard(&resp.blob.unwrap()).unwrap();
        let published = DeviceRegistry::verify(&blob, &bundle.verifying_key()).unwrap();
        assert!(!published.is_authorized("devB"));
        assert_eq!(published.registry_seq, 2);
    }

    #[tokio::test]
    async fn remove_without_existing_registry_errors() {
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default(); // no registry published
        let state = MemState::default();
        let err = remove_device(&hub, &state, &bundle, "acct-1", "devB")
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Registry(_)));
    }

    #[tokio::test]
    async fn removed_device_lanes_are_filtered_after_republish() {
        // devA + devX are both authorized; devX pushes a lane. devA then removes devX
        // and republishes the registry. A peer (devB) that refreshes must adopt the
        // shrunk registry and drop devX's lane via H3 — the end-to-end effect of removal.
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default();
        let state_a = MemState::default();
        seed_registry(&hub, &registry_for(&["devA", "devX"]), &bundle).await; // seq 1

        let eng_x = FakeEngine::new("devX");
        let state_x = MemState::default();
        eng_x.edit("book-x", "from the removed device", false);
        sync_once(&hub, &eng_x, &bundle, &state_x, &ctx("devX"))
            .await
            .unwrap();

        // devA removes devX (republishes seq 2 without devX).
        remove_device(&hub, &state_a, &bundle, "acct-1", "devX")
            .await
            .unwrap();

        // devB refreshes the registry inside the cycle → devX is no longer authorized.
        let eng_b = FakeEngine::new("devB");
        let state_b = MemState::default();
        let stats = refresh_then_sync(&hub, &eng_b, &bundle, &state_b, "acct-1", "devB")
            .await
            .unwrap();
        assert_eq!(
            stats.applied, 0,
            "the removed device's lane must be filtered after republish (H3)"
        );
        assert!(
            !eng_b.snapshot().iter().any(|(u, _, _)| u == "book-x"),
            "book-x from the removed device must not be applied"
        );
        assert_eq!(state_b.registry_seq("acct-1").await.unwrap(), 2);
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

    #[tokio::test]
    async fn refresh_then_sync_denies_an_enrolled_device_when_hub_has_no_registry() {
        // A device that has already adopted a signed registry (registry_seq > 0) must
        // refuse to sync if the hub now serves no registry, rather than silently
        // falling back to "accept all lanes" (which would let a hostile hub disable
        // the H3 device-authorization filter).
        let bundle = AccountKeyBundle::generate();
        let hub = MemHub::default(); // no registry published
        let state = MemState::default();
        state.set_registry_seq("acct-1", 1).await.unwrap();

        let eng = FakeEngine::new("devA");
        let err = refresh_then_sync(&hub, &eng, &bundle, &state, "acct-1", "devA")
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::Registry(_)));
    }

    // Production-stack variant: the SAME sync_once pipeline, driven by the REAL cr-sqlite engine over
    // the production sqlx + SeaORM stack (two in-memory cr-sqlite DBs) instead of the
    // in-memory fake. Validates that the real CRDT engine converges through our
    // encrypt/transport/cursor loop on the stack the app actually uses.
    // Runs only with `--features crsqlite` (needs the vendored extension).
    #[cfg(feature = "crsqlite")]
    #[tokio::test(flavor = "multi_thread")]
    async fn real_crsqlite_two_devices_converge() {
        use crate::services::crsqlite_engine::CrSqliteMergeEngine;
        use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Set, Statement};

        // Seed a book + author into a real-schema engine (both are CRRs), exercising
        // the multi-table path. `before_save` is bypassed by setting the uuid PK.
        async fn seed(eng: &CrSqliteMergeEngine, book_uuid: &str, title: &str, author_uuid: &str) {
            crate::models::book::ActiveModel {
                id: Set(book_uuid.to_owned()),
                title: Set(title.to_owned()),
                created_at: Set("2026-06-29T00:00:00Z".to_owned()),
                updated_at: Set("2026-06-29T00:00:00Z".to_owned()),
                ..Default::default()
            }
            .insert(eng.db())
            .await
            .unwrap();
            crate::models::author::ActiveModel {
                id: Set(author_uuid.to_owned()),
                name: Set(format!("author {author_uuid}")),
                created_at: Set("2026-06-29T00:00:00Z".to_owned()),
                updated_at: Set("2026-06-29T00:00:00Z".to_owned()),
            }
            .insert(eng.db())
            .await
            .unwrap();
        }
        async fn book_uuids(eng: &CrSqliteMergeEngine) -> Vec<String> {
            let rows = eng
                .db()
                .query_all(Statement::from_string(
                    eng.db().get_database_backend(),
                    "SELECT uuid FROM books ORDER BY uuid".to_owned(),
                ))
                .await
                .unwrap();
            rows.iter()
                .map(|r| r.try_get("", "uuid").unwrap())
                .collect()
        }

        let bundle = Arc::new(AccountKeyBundle::generate());
        let hub = Arc::new(MemHub::default());
        let eng_a = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let eng_b = CrSqliteMergeEngine::open_real_schema_in_memory()
            .await
            .unwrap();
        let state_a = MemState::default();
        let state_b = MemState::default();

        // Offline divergence across two CRR tables on two real cr-sqlite databases.
        seed(&eng_a, "book-1", "title from A", "author-1").await;
        seed(&eng_b, "book-1", "title from B", "author-2").await;
        crate::models::book::ActiveModel {
            id: Set("book-2".to_owned()),
            title: Set("only on B".to_owned()),
            created_at: Set("2026-06-29T00:00:00Z".to_owned()),
            updated_at: Set("2026-06-29T00:00:00Z".to_owned()),
            ..Default::default()
        }
        .insert(eng_b.db())
        .await
        .unwrap();

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

        // Both books propagated to both devices (cr-sqlite picks the book-1 LWW
        // winner by its own HLC; we assert the row set converges).
        let books_a = book_uuids(&eng_a).await;
        let books_b = book_uuids(&eng_b).await;
        assert_eq!(books_a, books_b, "real cr-sqlite engines must converge");
        assert_eq!(books_a, vec!["book-1".to_string(), "book-2".to_string()]);
        // The author rows (a second CRR table) converged too.
        let authors_a = crate::models::author::Entity::find()
            .all(eng_a.db())
            .await
            .unwrap()
            .len();
        let authors_b = crate::models::author::Entity::find()
            .all(eng_b.db())
            .await
            .unwrap()
            .len();
        assert_eq!(authors_a, 2, "both authors must propagate");
        assert_eq!(authors_b, 2);

        // Honour the cr-sqlite teardown contract.
        eng_a.finalize().await.unwrap();
        eng_b.finalize().await.unwrap();
    }
}
