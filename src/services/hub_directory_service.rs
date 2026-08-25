//! Hub Directory Service
//!
//! Manages outbound communication with the hub's public library directory (ADR-015).
//! Responsibilities:
//!   - Registering and updating the library's public profile
//!   - Pushing the local ISBN catalog to the hub cache
//!   - Browsing the hub directory
//!   - Managing follow relationships (send, approve, reject, unfollow)
//!   - Retrieving followed libraries' catalogs
//!   - Hub-mediated borrow requests (ADR-018)
//!   - Persisting local directory settings (node_id, write_token, visibility)

use reqwest::Client;
use sea_orm::{ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};

fn default_true() -> Option<bool> {
    Some(true)
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum HubDirectoryError {
    /// Network or transport failure
    Network(String),
    /// Hub returned a non-2xx status
    Hub(u16, String),
    /// Library is not yet registered with the hub directory
    NotRegistered,
    /// Local configuration or environment issue
    Config(String),
}

impl std::fmt::Display for HubDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "Network error: {e}"),
            Self::Hub(code, msg) => write!(f, "Hub error {code}: {msg}"),
            Self::NotRegistered => write!(f, "Not registered with hub directory"),
            Self::Config(e) => write!(f, "Configuration error: {e}"),
        }
    }
}

impl From<reqwest::Error> for HubDirectoryError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e.to_string())
    }
}

impl From<sea_orm::DbErr> for HubDirectoryError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Config(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Data transfer objects (hub API contract)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HubProfile {
    pub node_id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub book_count: i32,
    pub location_country: Option<String>,
    /// GeoNames populated-place ID opted into by the user (ADR-035).
    /// None when the user has not enabled "share my city".
    #[serde(default)]
    pub location_city_id: Option<i64>,
    pub requires_approval: bool,
    /// Whether this library accepts borrow requests from followers.
    #[serde(default = "default_true")]
    pub allow_borrowing: Option<bool>,
    pub last_seen_at: Option<String>,
    /// Returned once on first registration - must be stored locally.
    pub write_token: Option<String>,
    /// Total catalog views from followers (incremented by hub with cooldown).
    #[serde(default)]
    pub view_count: Option<i64>,
    /// X25519 public key (hex-encoded, 64 chars) for E2EE contact encryption.
    #[serde(default)]
    pub x25519_public_key: Option<String>,
    /// Public website URL (visible to all directory visitors).
    #[serde(default)]
    pub website: Option<String>,
    /// Hardware model name (e.g. "SM-A405FN", "iPhone14,2").
    #[serde(default)]
    pub device_model: Option<String>,
    /// SHA-256 hash of a platform-specific device identifier.
    #[serde(default)]
    pub device_fingerprint: Option<String>,
    /// Client app version reported at last register/heartbeat (e.g. "0.9.0+422").
    #[serde(default)]
    pub app_version: Option<String>,
    /// Relay credentials (returned only to authenticated requesters).
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub relay_mailbox_id: Option<String>,
    #[serde(default)]
    pub relay_write_token: Option<String>,
    /// JSON avatar configuration (DiceBear style + seed + customisation).
    #[serde(default)]
    pub avatar_config: Option<String>,
    /// One-time recovery code (returned once on first registration and on recovery).
    #[serde(default)]
    pub recovery_code: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HubFollow {
    pub id: i64,
    pub follower_node_id: String,
    pub followed_node_id: String,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    /// Display name of the follower (enriched by the hub for pending requests).
    #[serde(default)]
    pub follower_display_name: Option<String>,
    /// E2EE sealed blob: followed library's contact info, encrypted for this follower.
    #[serde(default)]
    pub encrypted_contact: Option<String>,
    /// X25519 public key of the follower (returned in pending/followers lists for encryption).
    #[serde(default)]
    pub follower_x25519_public_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HubCatalog {
    pub node_id: String,
    pub isbn_payload: String,
    /// Enriched catalog: JSON array of CatalogEntry objects. Absent for legacy pushes.
    #[serde(default)]
    pub catalog_payload: Option<String>,
    pub updated_at: String,
    pub expires_at: String,
}

/// A single entry in the enriched catalog (ISBN + title + author + optional cover).
///
/// `book_id` is the owner's local primary key. The current writer
/// (`hub_directory_sync_catalog`) always populates it because the hub's
/// cover GC (ADR-033) needs it to identify orphan `covers/{node}/{id}.jpg`
/// files. Kept Optional so the type still deserializes catalogs produced
/// by older clients (pre-ADR-033) that omitted the field.
///
/// On the wire the field is WRITTEN as `book_uuid`, not `book_id`: pre-uuid
/// builds declare `book_id: Option<i32>` and fail their whole catalog decode
/// on a string value, blanking this library for every not-yet-updated
/// follower. They ignore the unknown `book_uuid` key instead (missing
/// `book_id` → None), so both generations keep reading current catalogs.
/// Reading accepts both spellings (`alias`) because catalogs pushed by older
/// clients stay cached hub-side.
///
/// `added_at` carries the owner's `books.created_at` so every follower agrees
/// on whether an entry is recent (replaces the per-viewer `peer_books.first_seen_at`
/// heuristic, which was noisy because it flagged every entry as "new" on first
/// sync of a library). Optional so older clients still round-trip cleanly.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CatalogEntry {
    pub isbn: String,
    #[serde(
        default,
        rename = "book_uuid",
        alias = "book_id",
        deserialize_with = "deserialize_book_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub book_id: Option<String>,
    pub title: String,
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

/// Deserialize `book_id` from either a JSON string (current builds: the book
/// uuid) or a JSON number (pre-uuid builds: the integer row id). Catalogs from
/// both generations coexist on the hub, and without this tolerance a single
/// numeric `book_id` used to fail the whole enriched payload, downgrading the
/// library to a title-less ISBN-only catalog for every follower.
fn deserialize_book_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(i64),
    }
    Ok(
        Option::<StringOrNumber>::deserialize(deserializer)?.map(|v| match v {
            StringOrNumber::String(s) => s,
            StringOrNumber::Number(n) => n.to_string(),
        }),
    )
}

/// Decode the enriched `catalog_payload` entry by entry. A malformed entry
/// (unexpected field type, missing `title`, …) degrades to an ISBN-only entry
/// instead of failing the whole catalog: an all-or-nothing decode turned one
/// incompatible entry into a fully title-less library for every follower.
/// Entries with no recoverable ISBN are dropped. Errors only when the payload
/// itself is not a JSON array.
fn parse_catalog_entries(payload: &str) -> Result<Vec<CatalogEntry>, serde_json::Error> {
    let values: Vec<serde_json::Value> = serde_json::from_str(payload)?;
    Ok(values
        .into_iter()
        .filter_map(
            |value| match serde_json::from_value::<CatalogEntry>(value.clone()) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    let isbn = match value.get("isbn") {
                        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
                        Some(serde_json::Value::Number(n)) => n.to_string(),
                        _ => {
                            tracing::warn!("directory catalog entry dropped (no usable isbn): {e}");
                            return None;
                        }
                    };
                    tracing::warn!("directory catalog entry {isbn} degraded to ISBN-only: {e}");
                    Some(CatalogEntry {
                        isbn,
                        book_id: None,
                        title: String::new(),
                        author: None,
                        cover_url: None,
                        added_at: None,
                    })
                }
            },
        )
        .collect())
}

/// Result of `push_catalog`: whether the catalog was actually sent or the
/// push was skipped because the hub already has the same content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushCatalogOutcome {
    /// The catalog was sent and accepted (HTTP 200).
    Pushed,
    /// The push was short-circuited because the catalog hash matched the
    /// last successful push (no network round-trip).
    SkippedLocal,
    /// The catalog was sent but the hub returned 304 Not Modified (its
    /// stored catalog matches). The local hash is refreshed.
    SkippedRemote,
}

/// Max age (days) of the last confirmed network push before the local hash
/// fast path (ADR-027) is bypassed so the hub's cached-catalog TTL gets
/// refreshed.
///
/// The hub prunes `cached_catalogs` rows 7 days after the last push it
/// received (both 200 and 304 bump that TTL). The Flutter keep-alive triggers
/// a sync at most every 4 days and also refreshes its own timer on a
/// `SkippedLocal` outcome, so the worst-case gap between two real pushes is
/// just under (this window + 4 days). Two days keeps that below the 7-day
/// hub TTL with a day of margin.
const CATALOG_LOCAL_SKIP_MAX_AGE_DAYS: i64 = 2;

/// True when the last confirmed network push is recent enough that the hub's
/// cached-catalog TTL does not need a refresh yet, i.e. the local hash fast
/// path may skip the HTTP round-trip.
///
/// A missing or unparseable timestamp counts as stale so configs predating
/// the `last_catalog_pushed_at` column (migration 086) re-push once and
/// establish the baseline.
fn hub_catalog_recently_pushed(
    last_pushed_at: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(raw) = last_pushed_at else {
        return false;
    };
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return false;
    };
    now.signed_duration_since(parsed.with_timezone(&chrono::Utc))
        < chrono::Duration::days(CATALOG_LOCAL_SKIP_MAX_AGE_DAYS)
}

/// Compute a deterministic SHA-256 of the canonical catalog payload.
///
/// Returns a 64-char lowercase hex digest (unquoted) suitable for the
/// `catalog_hash` body field.
///
/// The inputs are length-prefixed to make the hash unambiguous regardless
/// of payload content (separators, null bytes, valid JSON strings that
/// happen to look like each other). Callers must pass the exact
/// `isbn_payload` / `catalog_payload` strings that will be POSTed and
/// must sort catalog entries beforehand to keep the digest stable
/// across calls.
pub fn compute_catalog_hash(isbn_payload: &str, catalog_payload: &str, book_count: i64) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let fields: [&[u8]; 2] = [isbn_payload.as_bytes(), catalog_payload.as_bytes()];
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.update(book_count.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// A hub-mediated borrow request (ADR-018).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HubBorrowRequest {
    pub id: i64,
    pub requester_node_id: String,
    pub lender_node_id: String,
    pub isbn: String,
    pub book_title: String,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    #[serde(default)]
    pub requester_display_name: Option<String>,
    #[serde(default)]
    pub lender_display_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Register / update params
// ---------------------------------------------------------------------------

/// Profile fields the caller controls.
///
/// `book_count` is deliberately absent: the public number is derived from the
/// catalog by [`HubDirectoryService::count_public_catalog_books`] at upsert
/// time, so the profile header and the catalog followers receive can never
/// disagree.
#[derive(Debug, Clone, Default)]
pub struct RegisterParams {
    pub node_id: String,
    pub display_name: String,
    pub is_listed: bool,
    pub requires_approval: bool,
    pub accept_from: String,
    pub description: Option<String>,
    pub location_country: Option<String>,
    /// GeoNames populated-place ID, opt-in (ADR-035). None means no city sharing.
    pub location_city_id: Option<i64>,
    pub allow_borrowing: bool,
    pub x25519_public_key: Option<String>,
    pub website: Option<String>,
    pub device_model: Option<String>,
    pub device_fingerprint: Option<String>,
    pub app_version: Option<String>,
    pub relay_url: Option<String>,
    pub relay_mailbox_id: Option<String>,
    pub relay_write_token: Option<String>,
    pub avatar_config: Option<String>,
}

// ---------------------------------------------------------------------------
// Public catalog scope
// ---------------------------------------------------------------------------

/// The books that reach the hub catalog, and therefore the public
/// `book_count`: owned, non-private copies carrying at least one identifier.
///
/// `private` books are excluded because the flag means exactly that: hidden
/// from network peers. Every other peer-facing read path already honours it
/// (`api/peer/search.rs`, the E2EE catalog responses in `api/e2ee.rs`); the
/// hub directory catalog is the one that did not, and it is the most exposed
/// of them, being served to followers without a live connection. The
/// comparison is `= false` rather than `!= true` so a row that somehow holds
/// NULL in this NOT NULL column (a corrupt replicated changeset) stays out of
/// the public catalog: failing closed is the safe direction for a privacy gate.
///
/// This gate is about what leaves the device towards *other people*. It has no
/// bearing on account sync between the user's own devices, which replicates
/// raw cr-sqlite changesets and never goes through this predicate: a private
/// book still reaches the owner's second device.
///
/// A row with neither ISBN nor title is unusable for a follower (nothing to
/// render, nothing to match a wishlist against) and is never pushed. A row
/// carrying only one of the two IS pushed and IS counted: a title-less book
/// still lists under its ISBN on the viewer side, and hiding it would make
/// the missing title invisible to the owner.
///
/// Single source of truth, shared by the catalog builder in
/// `api/frb/hub_catalog.rs` and by
/// [`HubDirectoryService::count_public_catalog_books`], so the catalog that
/// is pushed and the count that is announced cannot drift apart.
pub fn public_catalog_condition() -> Condition {
    use crate::models::book::Column;
    Condition::all()
        .add(Column::Owned.eq(true))
        .add(Column::Private.eq(false))
        .add(
            Condition::any()
                .add(
                    Condition::all()
                        .add(Column::Isbn.is_not_null())
                        .add(Column::Isbn.ne("")),
                )
                .add(Column::Title.ne("")),
        )
}

// ---------------------------------------------------------------------------
// Local config (stored in hub_directory_config, singleton row)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DirectoryConfig {
    pub node_id: String,
    pub write_token: String,
    pub is_listed: bool,
    pub requires_approval: bool,
    pub accept_from: String,
    pub allow_borrowing: bool,
    pub recovery_code: Option<String>,
    /// SHA-256 hex digest of the last catalog payload successfully
    /// pushed to (or confirmed by) the hub. Used to skip redundant
    /// uploads (ADR-027). None until the first successful push.
    pub last_catalog_hash: Option<String>,
    /// RFC3339 instant of the last catalog push that actually reached the
    /// hub (HTTP 200 or 304). The ADR-027 fast path only skips the network
    /// while this is fresh, so the hub's cached-catalog TTL keeps getting
    /// bumped even when the catalog never changes locally. None until the
    /// first confirmed push (and reset together with the hash on recovery).
    pub last_catalog_pushed_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// One cover this run already pushed to the hub, so the next catalog sync can
/// skip the read, the re-encode and the POST.
struct UploadedCover {
    /// Identity of the file that was sent. Modification time alone is not
    /// enough: it has second resolution on some filesystems, and a photo
    /// replaced within the same second would then be masked.
    modified: std::time::SystemTime,
    len: u64,
    /// The URL the upload returned, replayed verbatim on a hit. The catalog
    /// builder fills its entry from this value, so a hit MUST answer with the
    /// URL and not merely report success: `None` would push a catalog with no
    /// cover and leave every follower on a placeholder.
    hub_url: String,
}

/// What a cover upload attempt actually did, so the caller can tell a real POST
/// from a replayed cache hit and skip the bookkeeping a hit cannot have
/// invalidated.
pub enum CoverUpload {
    /// The file was read, re-encoded and POSTed to the hub.
    Sent(String),
    /// Already sent during this run and untouched since: nothing left the
    /// device, and the URL is the one the original upload returned.
    AlreadySent(String),
    /// The row names a local custom cover whose bytes are not on this device.
    /// Nothing was read, nothing was sent, and nothing failed.
    Missing,
}

pub struct HubDirectoryService {
    http_client: Client,
    /// Covers already uploaded during this run, keyed by book id.
    ///
    /// Deliberately in memory and not persisted (ADR-044 A.6 defers the shared
    /// marker). The catalog push is debounced 5s behind every book edit, so a
    /// cataloguing session used to re-send every custom cover once per edit.
    /// This collapses that to once per run. Dying with the process is the
    /// feature, not a limitation: if the hub ever loses a blob, the next launch
    /// re-uploads it with no bookkeeping to repair.
    ///
    /// Bounded by construction, one entry per catalogued book carrying a custom
    /// cover, so it cannot outgrow the `entries` vector the same sync builds. No
    /// eviction policy on top: capping it would mean dropping entries a sync
    /// still needs and re-uploading them, which is the cost this exists to
    /// avoid.
    uploaded_covers: std::sync::Mutex<std::collections::HashMap<String, UploadedCover>>,
}

impl HubDirectoryService {
    pub fn new() -> Self {
        let http_client = Client::builder()
            .user_agent("BiblioGenius/1.0")
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            http_client,
            uploaded_covers: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Drop every remembered upload. Called when the hub registration is
    /// purged: the node id is part of the URLs handed back on a hit, and the
    /// hub no longer holds any of the blobs.
    pub fn forget_uploaded_covers(&self) {
        if let Ok(mut cache) = self.uploaded_covers.lock() {
            cache.clear();
        }
    }

    /// Forget one book's remembered upload, so the next sync really re-sends it.
    ///
    /// Called whenever an upload attempt fails. It keeps the invariant a cache
    /// hit relies on: a hit means the last attempt for this exact file
    /// succeeded, so the failure flag it cleared is still clear. Without this,
    /// a cover that failed once (file briefly unreadable) and then came back
    /// unchanged would hit the cache, skip the flag clearing, and keep a
    /// "cover not synced" badge forever on a cover the hub actually holds.
    fn forget_uploaded_cover(&self, book_id: &str) {
        if let Ok(mut cache) = self.uploaded_covers.lock() {
            cache.remove(book_id);
        }
    }

    /// The URL already returned for this exact file, if it was uploaded during
    /// this run and has not been touched since.
    ///
    /// The entry is also checked against the hub currently configured. The
    /// remembered URL carries the hub host and the node id, and a registration
    /// can be purged mid-run: `api/peer/relay_config.rs` drops the directory
    /// config when the hub URL changes, and the re-registration that follows
    /// yields a different node id. Replaying the old URL would then publish, to
    /// the new hub's followers, cover links pointing at a node that holds none
    /// of the blobs, with no upload ever correcting it for the rest of the run.
    ///
    /// This covers every purge path that can strike mid-run with books still in
    /// the catalog: the hub-URL changes are caught here, and the explicit
    /// `hub_directory_purge_config` clears the cache outright. The startup path
    /// runs before anything is cached, and the full app reset leaves no book to
    /// look up.
    fn cached_cover_url(
        &self,
        book_id: &str,
        modified: std::time::SystemTime,
        len: u64,
    ) -> Option<String> {
        let current_hub = Self::hub_base_url().ok()?;
        let prefix = format!("{current_hub}/api/directory/");
        let cache = self.uploaded_covers.lock().ok()?;
        let hit = cache.get(book_id)?;
        (hit.modified == modified && hit.len == len && hit.hub_url.starts_with(&prefix))
            .then(|| hit.hub_url.clone())
    }

    fn remember_uploaded_cover(
        &self,
        book_id: &str,
        modified: std::time::SystemTime,
        len: u64,
        hub_url: &str,
    ) {
        if let Ok(mut cache) = self.uploaded_covers.lock() {
            cache.insert(
                book_id.to_string(),
                UploadedCover {
                    modified,
                    len,
                    hub_url: hub_url.to_string(),
                },
            );
        }
    }

    // -----------------------------------------------------------------------
    // Hub URL: reads HUB_URL env var, which is kept in sync with
    // my_relay_config.relay_url (set at startup and on relay setup).
    // The .env value is only used as initial default before relay is configured.
    // -----------------------------------------------------------------------

    pub(crate) fn hub_base_url() -> Result<String, HubDirectoryError> {
        std::env::var("HUB_URL")
            .map(|u| u.trim_end_matches('/').to_string())
            .map_err(|_| {
                HubDirectoryError::Config("HUB_URL environment variable not set".to_string())
            })
    }

    // -----------------------------------------------------------------------
    // Local config persistence
    // -----------------------------------------------------------------------

    pub async fn get_config(
        db: &DatabaseConnection,
    ) -> Result<Option<DirectoryConfig>, HubDirectoryError> {
        let backend = db.get_database_backend();
        let result = db
            .query_one(Statement::from_string(
                backend,
                "SELECT node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, recovery_code, last_catalog_hash, last_catalog_pushed_at
                 FROM hub_directory_config WHERE id = 1"
                    .to_owned(),
            ))
            .await?;

        let Some(row) = result else {
            return Ok(None);
        };

        Ok(Some(DirectoryConfig {
            node_id: row.try_get("", "node_id")?,
            write_token: row.try_get("", "write_token")?,
            is_listed: row.try_get::<i32>("", "is_listed")? != 0,
            requires_approval: row.try_get::<i32>("", "requires_approval")? != 0,
            accept_from: row.try_get("", "accept_from")?,
            allow_borrowing: row.try_get::<i32>("", "allow_borrowing").unwrap_or(1) != 0,
            recovery_code: row.try_get::<String>("", "recovery_code").ok(),
            last_catalog_hash: row.try_get::<String>("", "last_catalog_hash").ok(),
            last_catalog_pushed_at: row.try_get::<String>("", "last_catalog_pushed_at").ok(),
        }))
    }

    async fn save_config(
        db: &DatabaseConnection,
        config: &DirectoryConfig,
    ) -> Result<(), HubDirectoryError> {
        let now = chrono::Utc::now().to_rfc3339();
        let backend = db.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            format!(
                "INSERT INTO hub_directory_config
                     (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, recovery_code, created_at, updated_at)
                 VALUES (1, '{node_id}', '{write_token}', {is_listed}, {requires_approval}, '{accept_from}', {allow_borrowing}, {recovery_code}, '{now}', '{now}')
                 ON CONFLICT(id) DO UPDATE SET
                     node_id           = excluded.node_id,
                     write_token       = excluded.write_token,
                     is_listed         = excluded.is_listed,
                     requires_approval = excluded.requires_approval,
                     accept_from       = excluded.accept_from,
                     allow_borrowing   = excluded.allow_borrowing,
                     recovery_code     = COALESCE(excluded.recovery_code, hub_directory_config.recovery_code),
                     updated_at        = excluded.updated_at",
                node_id          = config.node_id.replace('\'', "''"),
                write_token      = config.write_token.replace('\'', "''"),
                is_listed        = if config.is_listed { 1 } else { 0 },
                requires_approval = if config.requires_approval { 1 } else { 0 },
                accept_from      = config.accept_from.replace('\'', "''"),
                allow_borrowing  = if config.allow_borrowing { 1 } else { 0 },
                recovery_code    = config.recovery_code.as_ref()
                    .map(|c| format!("'{}'", c.replace('\'', "''")))
                    .unwrap_or_else(|| "NULL".to_string()),
                now              = now,
            ),
        ))
        .await?;
        Ok(())
    }

    /// Persist the state of the last catalog push confirmed by the hub: the
    /// payload hash (so the next identical sync can skip the HTTP round-trip,
    /// ADR-027) and the instant of confirmation (200 or 304). That timestamp
    /// bounds the fast path: once it goes stale the next sync re-pushes so
    /// the hub's cached-catalog TTL is refreshed.
    ///
    /// Passing `None` resets both, which forces the next sync to re-push
    /// unconditionally (used after recovery where the hub's cached catalog
    /// may have been lost).
    pub(crate) async fn record_catalog_push_state(
        db: &DatabaseConnection,
        hash: Option<&str>,
    ) -> Result<(), HubDirectoryError> {
        let now = chrono::Utc::now().to_rfc3339();
        let pushed_at: Option<String> = hash.map(|_| now.clone());
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE hub_directory_config
             SET last_catalog_hash = ?,
                 last_catalog_pushed_at = ?,
                 updated_at = ?
             WHERE id = 1",
            [
                hash.map(str::to_string).into(),
                pushed_at.into(),
                now.into(),
            ],
        ))
        .await?;
        Ok(())
    }

    /// Returns the current write_token for Keychain backup (reinstall recovery).
    /// Returns None if not yet registered.
    pub async fn get_write_token(
        db: &DatabaseConnection,
    ) -> Result<Option<String>, HubDirectoryError> {
        let backend = db.get_database_backend();
        let result = db
            .query_one(Statement::from_string(
                backend,
                "SELECT write_token FROM hub_directory_config WHERE id = 1".to_owned(),
            ))
            .await?;
        Ok(result.and_then(|row| row.try_get::<String>("", "write_token").ok()))
    }

    /// Imports a write_token recovered from Keychain after reinstall.
    /// Creates a minimal config row so the next register_or_update() can
    /// authenticate with the hub instead of failing with 401.
    pub async fn import_write_token(
        db: &DatabaseConnection,
        node_id: &str,
        write_token: &str,
    ) -> Result<(), HubDirectoryError> {
        let now = chrono::Utc::now().to_rfc3339();
        let backend = db.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            format!(
                "INSERT INTO hub_directory_config
                     (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, created_at, updated_at)
                 VALUES (1, '{node_id}', '{write_token}', 0, 1, 'everyone', 1, '{now}', '{now}')
                 ON CONFLICT(id) DO UPDATE SET
                     node_id     = excluded.node_id,
                     write_token = excluded.write_token,
                     updated_at  = excluded.updated_at",
                node_id     = node_id.replace('\'', "''"),
                write_token = write_token.replace('\'', "''"),
                now         = now,
            ),
        ))
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Profile
    // -----------------------------------------------------------------------

    /// Counts the books the hub catalog exposes (see
    /// [`public_catalog_condition`]). This is the number advertised as the
    /// profile's `book_count`, so the library header never announces books a
    /// follower cannot find in the catalog.
    pub async fn count_public_catalog_books(
        db: &DatabaseConnection,
    ) -> Result<i64, HubDirectoryError> {
        use crate::models::book::Entity as BookEntity;
        use sea_orm::{EntityTrait, PaginatorTrait, QueryFilter};

        let count = BookEntity::find()
            .filter(public_catalog_condition())
            .count(db)
            .await?;
        Ok(count as i64)
    }

    /// Build the JSON body for a hub directory register/update request.
    ///
    /// `book_count` is passed separately from `params`: it is derived from the
    /// catalog, not supplied by the caller (see [`RegisterParams`]).
    ///
    /// Most optional fields follow "Some = set, None = leave alone": the key
    /// is only added to the body when the caller passes a value. The hub's
    /// upsert handler uses array_key_exists, so absent keys preserve the
    /// stored value.
    ///
    /// `location_city_id` deliberately breaks that pattern: ADR-035 §8
    /// requires that toggling off "Partager ma ville" clears the hub side
    /// immediately. We therefore always include the key, sending JSON null
    /// when the caller passes None so the hub overwrites the stored value.
    fn build_register_body(params: &RegisterParams, book_count: i64) -> serde_json::Value {
        let mut body = serde_json::json!({
            "node_id":           params.node_id,
            "display_name":      params.display_name,
            "book_count":        book_count,
            "is_listed":         params.is_listed,
            "requires_approval": params.requires_approval,
            "accept_from":       params.accept_from,
            "allow_borrowing":   params.allow_borrowing,
        });

        if let Some(ref desc) = params.description {
            body["description"] = serde_json::Value::String(desc.clone());
        }
        if let Some(ref country) = params.location_country {
            body["location_country"] = serde_json::Value::String(country.clone());
        }
        // ADR-035 §8: always include the key so None propagates as a clear.
        body["location_city_id"] = match params.location_city_id {
            Some(id) => serde_json::Value::Number(id.into()),
            None => serde_json::Value::Null,
        };
        if let Some(ref key) = params.x25519_public_key {
            body["x25519_public_key"] = serde_json::Value::String(key.clone());
        }
        if let Some(ref url) = params.website {
            body["website"] = serde_json::Value::String(url.clone());
        }
        if let Some(ref model) = params.device_model {
            body["device_model"] = serde_json::Value::String(model.clone());
        }
        if let Some(ref fp) = params.device_fingerprint {
            body["device_fingerprint"] = serde_json::Value::String(fp.clone());
        }
        if let Some(ref v) = params.app_version {
            body["app_version"] = serde_json::Value::String(v.clone());
        }
        if let Some(ref url) = params.relay_url {
            body["relay_url"] = serde_json::Value::String(url.clone());
        }
        if let Some(ref mid) = params.relay_mailbox_id {
            body["relay_mailbox_id"] = serde_json::Value::String(mid.clone());
        }
        if let Some(ref wt) = params.relay_write_token {
            body["relay_write_token"] = serde_json::Value::String(wt.clone());
        }
        if let Some(ref ac) = params.avatar_config
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(ac)
        {
            body["avatar_config"] = val;
        }

        body
    }

    /// Registers the library with the hub directory (first call) or updates its profile.
    /// On first registration, the hub returns a write_token that is persisted locally.
    pub async fn register_or_update(
        &self,
        db: &DatabaseConnection,
        params: RegisterParams,
    ) -> Result<DirectoryConfig, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;
        let existing = Self::get_config(db).await?;

        // The profile upsert and the catalog push both write `book_count` on
        // the hub, last one wins. Deriving it from the catalog here keeps the
        // two writers in agreement whichever lands last.
        let book_count = Self::count_public_catalog_books(db).await?;
        let body = Self::build_register_body(&params, book_count);

        let initial_token = existing.as_ref().map(|c| c.write_token.clone());
        let has_auth = initial_token.is_some();

        let provenance = crate::services::relay_session::classify_mailbox_provenance(
            params.relay_mailbox_id.as_deref(),
        );

        tracing::info!(
            "Hub directory: register_or_update node_id={} hub={} auth={} relay_mailbox={} mailbox_fresh={}",
            &params.node_id[..12.min(params.node_id.len())],
            hub_url,
            has_auth,
            params.relay_mailbox_id.as_deref().unwrap_or("none"),
            matches!(
                provenance,
                crate::services::relay_session::MailboxProvenance::Fresh
            ),
        );

        if provenance == crate::services::relay_session::MailboxProvenance::Restored {
            // Diagnostic: the mailbox_uuid about to be advertised to the hub
            // was loaded from my_relay_config at startup, not minted this
            // session. If the hub has since purged it (device-fingerprint
            // dedup, admin purge, orphan cleanup) peers will silently hit
            // "deposit to non-existent mailbox". poll_inner() auto-recreates
            // on 404 but only if the poller is actually running against this
            // hub; this WARN surfaces the risk at the source so it can be
            // correlated with hub-side dashboard counters.
            let mid = params.relay_mailbox_id.as_deref().unwrap_or("");
            tracing::warn!(
                "Hub directory: advertising relay_mailbox_id={} restored from my_relay_config \
                 (not created this session) - may be stale if hub has purged it",
                &mid[..12.min(mid.len())],
            );
        }

        let response = self
            .send_profile_upsert(&hub_url, &body, initial_token.as_deref())
            .await?;

        // Self-heal path: a 401 on an existing profile usually means the
        // local write_token no longer matches the hub (e.g. the client was
        // reinstalled, or an older build wiped hub_directory_config during
        // a same-URL relay re-setup). If a recovery_code is stored locally
        // (migration 064+), exchange it for a fresh write_token via
        // /recover and retry the upsert once. All other 4xx/5xx bubble up.
        let (response, recovered) = if response.status().as_u16() == 401
            && let Some(ref cfg) = existing
            && let Some(recovery_code) = cfg.recovery_code.clone()
        {
            let _ = response.text().await; // drain for logging hygiene
            tracing::warn!(
                "Hub directory: 401 on profile upsert, attempting auto-recovery via stored recovery_code"
            );
            match self.recover(db, &params.node_id, &recovery_code).await {
                Ok(recovered) => {
                    tracing::info!(
                        "Hub directory: auto-recovery succeeded, retrying profile upsert"
                    );
                    let retry = self
                        .send_profile_upsert(&hub_url, &body, Some(&recovered.write_token))
                        .await?;
                    (retry, Some(recovered))
                }
                Err(e) => {
                    tracing::warn!("Hub directory: auto-recovery failed: {e}");
                    return Err(HubDirectoryError::Hub(
                        401,
                        "Unauthorized; auto-recovery failed".to_string(),
                    ));
                }
            }
        } else {
            (response, None)
        };

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            tracing::warn!("Hub directory: register_or_update failed {status}: {msg}");
            return Err(HubDirectoryError::Hub(status, msg));
        }

        tracing::info!("Hub directory: register_or_update succeeded (status={status})");

        let profile: HubProfile = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;

        let write_token = profile
            .write_token
            .or_else(|| recovered.as_ref().map(|c| c.write_token.clone()))
            .or_else(|| existing.as_ref().map(|c| c.write_token.clone()))
            .ok_or_else(|| {
                HubDirectoryError::Config("Hub did not return write_token".to_string())
            })?;

        // After auto-recovery, keep the fresh recovery_code from /recover if
        // the profile response didn't supply one; the previous code is now
        // burned on the hub and must not be re-persisted.
        let recovery_code = profile
            .recovery_code
            .or_else(|| recovered.as_ref().and_then(|c| c.recovery_code.clone()));

        // recover() resets last_catalog_hash to force a fresh push (ADR-027).
        // Outside the recovery path, keep whatever we had before.
        let last_catalog_hash = if recovered.is_some() {
            None
        } else {
            existing.as_ref().and_then(|c| c.last_catalog_hash.clone())
        };
        let last_catalog_pushed_at = if recovered.is_some() {
            None
        } else {
            existing
                .as_ref()
                .and_then(|c| c.last_catalog_pushed_at.clone())
        };

        let config = DirectoryConfig {
            node_id: params.node_id,
            write_token,
            is_listed: params.is_listed,
            requires_approval: params.requires_approval,
            accept_from: params.accept_from,
            allow_borrowing: params.allow_borrowing,
            recovery_code,
            last_catalog_hash,
            last_catalog_pushed_at,
        };

        Self::save_config(db, &config).await?;
        Ok(config)
    }

    /// POST the profile upsert body to `/api/directory/profile`, optionally
    /// carrying a Bearer token. Factored out so the 401 auto-recovery path
    /// can replay the request with fresh credentials without duplicating
    /// the body construction.
    async fn send_profile_upsert(
        &self,
        hub_url: &str,
        body: &serde_json::Value,
        bearer_token: Option<&str>,
    ) -> Result<reqwest::Response, HubDirectoryError> {
        let mut req = self
            .http_client
            .post(format!("{hub_url}/api/directory/profile"));
        if let Some(token) = bearer_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        req.json(body).send().await.map_err(|e| {
            tracing::warn!("Hub directory: network error: {e}");
            HubDirectoryError::Network(e.to_string())
        })
    }

    /// Returns the locally stored recovery code, if any.
    pub async fn get_recovery_code(
        db: &DatabaseConnection,
    ) -> Result<Option<String>, HubDirectoryError> {
        let backend = db.get_database_backend();
        let result = db
            .query_one(Statement::from_string(
                backend,
                "SELECT recovery_code FROM hub_directory_config WHERE id = 1".to_owned(),
            ))
            .await?;
        Ok(result.and_then(|row| row.try_get::<String>("", "recovery_code").ok()))
    }

    /// Recovers a hub profile using a one-time recovery code.
    /// On success: stores the new write_token + recovery_code locally.
    pub async fn recover(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
        recovery_code: &str,
    ) -> Result<DirectoryConfig, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;

        let body = serde_json::json!({
            "node_id": node_id,
            "recovery_code": recovery_code,
        });

        let response = self
            .http_client
            .post(format!("{hub_url}/api/directory/recover"))
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        let profile: HubProfile = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;

        let write_token = profile.write_token.ok_or_else(|| {
            HubDirectoryError::Config("Hub did not return write_token on recovery".to_string())
        })?;

        // Read existing config to preserve local settings (is_listed, etc.)
        let existing = Self::get_config(db).await?.unwrap_or(DirectoryConfig {
            node_id: node_id.to_string(),
            write_token: String::new(),
            is_listed: false,
            requires_approval: true,
            accept_from: "everyone".to_string(),
            allow_borrowing: true,
            recovery_code: None,
            last_catalog_hash: None,
            last_catalog_pushed_at: None,
        });

        // On recovery the hub's cached catalog may have been dropped or
        // drifted; clear the local hash so the next sync re-pushes
        // unconditionally (ADR-027).
        let config = DirectoryConfig {
            node_id: node_id.to_string(),
            write_token,
            is_listed: existing.is_listed,
            requires_approval: existing.requires_approval,
            accept_from: existing.accept_from,
            allow_borrowing: existing.allow_borrowing,
            recovery_code: profile.recovery_code,
            last_catalog_hash: None,
            last_catalog_pushed_at: None,
        };

        Self::save_config(db, &config).await?;
        // save_config preserves existing columns that aren't in its SET list;
        // the catalog push state (hash + pushed_at) is among them. Force a
        // reset here so the next sync re-pushes (hub's CachedCatalog may
        // have been lost/expired).
        Self::record_catalog_push_state(db, None).await?;
        tracing::info!("Hub: profile recovered via recovery code");
        Ok(config)
    }

    // -----------------------------------------------------------------------
    // Catalog cache
    // -----------------------------------------------------------------------

    /// Pushes the local catalog to the hub cache.
    ///
    /// Sends both the legacy ISBN list and enriched catalog entries (ISBN + title + author).
    /// Only meaningful for open libraries (requires_approval=false).
    ///
    /// Entries are sorted by `(isbn, book_id)` before serialization so the
    /// SHA-256 digest used for skip detection is stable across calls with
    /// the same logical content (ADR-027). The sorted order is also what
    /// gets sent to the hub, so peers always see a deterministic layout.
    ///
    /// Returns [`PushCatalogOutcome`] indicating whether the hub was
    /// actually contacted or the push was short-circuited.
    pub async fn push_catalog(
        &self,
        db: &DatabaseConnection,
        entries: &[CatalogEntry],
        book_count: i64,
    ) -> Result<PushCatalogOutcome, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        // Sort entries for hash determinism. Cheap on typical library sizes
        // (<1000 books). Cloning Strings is avoided by sorting a Vec of refs.
        let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.isbn.cmp(&b.isbn).then_with(|| a.book_id.cmp(&b.book_id)));

        // Legacy field: plain ISBN list for backward-compatible hubs
        let isbn_list: Vec<&str> = sorted.iter().map(|e| e.isbn.as_str()).collect();
        let isbn_payload = serde_json::to_string(&isbn_list)
            .map_err(|e| HubDirectoryError::Config(e.to_string()))?;

        // Enriched field: full catalog entries
        let catalog_payload =
            serde_json::to_string(&sorted).map_err(|e| HubDirectoryError::Config(e.to_string()))?;

        let catalog_hash = compute_catalog_hash(&isbn_payload, &catalog_payload, book_count);

        // Fast path: same hash as last successful push → no round-trip.
        // Only allowed while the hub confirmed a push recently: the hub
        // prunes cached catalogs on a TTL, and skipping here forever (a
        // catalog that never changes locally) would let that TTL lapse and
        // leave the directory fallback empty for peers that cannot reach us
        // live. Once stale, fall through to a real push; the hub answers
        // 304 and bumps its TTL, so the keep-alive stays cheap.
        if cfg.last_catalog_hash.as_deref() == Some(catalog_hash.as_str()) {
            if hub_catalog_recently_pushed(
                cfg.last_catalog_pushed_at.as_deref(),
                chrono::Utc::now(),
            ) {
                tracing::debug!(
                    target: "hub_directory",
                    "push_catalog: skipped (local hash match)"
                );
                return Ok(PushCatalogOutcome::SkippedLocal);
            }
            tracing::info!(
                target: "hub_directory",
                "push_catalog: hash unchanged but hub TTL refresh due, pushing"
            );
        }

        let response = self
            .http_client
            .post(format!("{hub_url}/api/directory/catalog"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&serde_json::json!({
                "isbn_payload": isbn_payload,
                "catalog_payload": catalog_payload,
                "book_count": book_count,
                "catalog_hash": catalog_hash,
            }))
            .send()
            .await?;

        let status = response.status().as_u16();

        // 304 Not Modified: hub's stored catalog already matches this hash.
        // Persist it locally so subsequent pushes can short-circuit.
        if status == 304 {
            Self::record_catalog_push_state(db, Some(&catalog_hash)).await?;
            tracing::debug!(
                target: "hub_directory",
                "push_catalog: skipped (hub returned 304)"
            );
            return Ok(PushCatalogOutcome::SkippedRemote);
        }

        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        // 2xx success: persist the hash so the next identical push skips.
        Self::record_catalog_push_state(db, Some(&catalog_hash)).await?;
        Ok(PushCatalogOutcome::Pushed)
    }

    /// DEBUG (catalog desync investigation): best-effort beacon so a failing
    /// catalog sync self-reports into the hub's `hub_events` table (retrievable
    /// via DB backups) without access to the device log. Swallows every error:
    /// observability must never change the sync outcome or surface to the user.
    pub async fn report_sync_diag(
        &self,
        db: &DatabaseConnection,
        phase: &str,
        ok: bool,
        detail: &str,
    ) {
        let Ok(Some(cfg)) = Self::get_config(db).await else {
            return;
        };
        let Ok(hub_url) = Self::hub_base_url() else {
            return;
        };
        // Cap detail so the beacon and the stored row stay small.
        let detail: String = detail.chars().take(300).collect();
        let _ = self
            .http_client
            .post(format!("{hub_url}/api/directory/catalog/diag"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&serde_json::json!({
                "phase": phase,
                "ok": ok,
                "detail": detail,
            }))
            .send()
            .await;
    }

    /// Uploads a cover thumbnail to the hub.
    ///
    /// Returns the public URL where the cover can be fetched.
    pub async fn upload_cover(
        &self,
        db: &DatabaseConnection,
        book_id: &str,
        jpeg_bytes: Vec<u8>,
    ) -> Result<String, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let url = format!("{hub_url}/api/directory/{}/covers/{book_id}", cfg.node_id);

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .header("Content-Type", "image/jpeg")
            .body(jpeg_bytes)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        Ok(format!(
            "{hub_url}/api/directory/{}/covers/{book_id}",
            cfg.node_id
        ))
    }

    /// Deletes a previously uploaded cover thumbnail from the hub.
    ///
    /// Called when a book is permanently removed from the local library
    /// so the hub storage does not keep growing with orphaned covers.
    /// Safe to call for books that never had a hub cover: the hub
    /// returns `204 No Content` for missing files.
    ///
    /// Not called on cover replacement (re-upload overwrites the same
    /// path on the hub, so no cleanup is needed).
    pub async fn delete_cover(
        &self,
        db: &DatabaseConnection,
        book_id: &str,
    ) -> Result<(), HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let url = format!("{hub_url}/api/directory/{}/covers/{book_id}", cfg.node_id);

        let response = self
            .http_client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        Ok(())
    }

    /// Records a failed hub cover upload so the owner's UI can surface a
    /// warning badge until the next sync retry succeeds. Side-effect only:
    /// DB errors are logged and swallowed so a bookkeeping failure never
    /// aborts the surrounding sync loop.
    pub async fn mark_hub_cover_upload_failure(db: &DatabaseConnection, book_id: &str) {
        // Device-local flag: stored in `book_local`, never on the `books` CRR
        // (ADR-044).
        let now = chrono::Utc::now().to_rfc3339();
        if let Err(e) =
            crate::infrastructure::book_local::set_cover_upload_failed_at(db, book_id, &now).await
        {
            tracing::warn!("failed to mark hub_cover_upload_failed_at for book {book_id}: {e}");
        }
    }

    /// Clears the pending-failure flag after a successful hub cover upload.
    /// Side-effect only: DB errors are logged and swallowed.
    pub async fn clear_hub_cover_upload_failure(db: &DatabaseConnection, book_id: &str) {
        if let Err(e) =
            crate::infrastructure::book_local::clear_cover_upload_failed_at(db, book_id).await
        {
            tracing::warn!("failed to clear hub_cover_upload_failed_at for book {book_id}: {e}");
        }
    }

    /// Resets the pending-failure flag on every book. Called when the library
    /// unregisters from the hub so stale warning badges do not survive a
    /// purge / re-registration cycle.
    pub async fn reset_all_hub_cover_upload_failures(db: &DatabaseConnection) {
        if let Err(e) = crate::infrastructure::book_local::clear_all_cover_upload_failures(db).await
        {
            tracing::warn!("failed to reset hub_cover_upload_failed_at: {e}");
        }
    }

    /// Reads a local cover file, resizes it to a JPEG thumbnail and uploads it
    /// to the hub, UNLESS this run already sent that exact file. Shared pipeline
    /// so LAN peer responses and hub-stored covers stay pixel-for-pixel
    /// identical.
    ///
    /// Returns `CoverUpload::Sent` when the bytes really left the device, and
    /// `CoverUpload::AlreadySent` when the URL is replayed from `uploaded_covers`
    /// and nothing was read, re-encoded or POSTed. Both carry the same hub URL,
    /// so a caller that only wants the URL may treat them alike. A caller doing
    /// bookkeeping around the upload must not: see `process_local_cover_upload`,
    /// where clearing the failure flag on a replay would be a write with nothing
    /// to write about, and where a failed attempt has to evict the entry to keep
    /// that reasoning true.
    ///
    /// `stored_cover` is the raw `books.cover_url` value and `covers_dir` the
    /// directory covers actually live in on this device (`None` in
    /// server-binary mode). In app mode `stored_cover` never decides WHERE the
    /// read happens: reading it directly breaks on iOS as soon as the
    /// data-container UUID changes, and the resulting ENOENT would flag every
    /// custom cover as un-syncable forever.
    pub async fn resize_and_upload_cover(
        &self,
        db: &DatabaseConnection,
        book_id: &str,
        covers_dir: Option<&std::path::Path>,
        stored_cover: &str,
    ) -> Result<CoverUpload, String> {
        // `books.cover_url` is replicated raw across devices (ADR-011), so its
        // value is not necessarily one this device wrote: a paired device can
        // store any readable path there, and these bytes are POSTed to the hub
        // for every follower to fetch. Refusing `..` only covers half of that
        // class, since `/etc/hosts` carries no relative segment.
        //
        // So in app mode the stored value is ignored entirely and the path is
        // derived from the book's own identity, the strict pattern
        // `services/cover_sync.rs` already applies. No legitimate case changes:
        // this device's own custom covers are always `<book_id>.jpg` under
        // `covers_dir`, which is exactly what the derivation yields, and any
        // other value names a file that is absent locally, so it fails now as
        // it failed before.
        //
        // In server-binary mode there is no covers directory to derive from and
        // paths are stable, so the stored value is read as given, minus the
        // traversal segments the peer-facing endpoint also refuses. That leaves
        // the absolute-path half of the class open on that build ALONE, and
        // knowingly: it has no covers directory to key a derivation on, and it
        // does not run on the devices where a paired peer can write the column.
        // Do not read the narrower guard here as an oversight to copy outward.
        let path = match covers_dir {
            // `book_id` becomes a filename component, so an absolute or dotted
            // id would escape `covers_dir` through `join`. The derivation and
            // its allowlist live in `cover_url` so the read side asks about the
            // very file this reads, and neither can drift from the other.
            Some(dir) => crate::utils::cover_url::own_local_cover_path(dir, book_id)
                .ok_or_else(|| format!("refused unsafe book id {book_id}"))?,
            None => {
                if stored_cover.split(['/', '\\']).any(|seg| seg == "..") {
                    return Err(format!("refused traversal path for book {book_id}"));
                }
                std::path::PathBuf::from(stored_cover)
            }
        };
        // File identity, read before the bytes: a cover already sent this run
        // and untouched since needs no read, no re-encode and no POST. `None`
        // here just means the metadata call failed, in which case the read
        // below reports the real error.
        let identity = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|meta| meta.modified().ok().map(|when| (when, meta.len())));

        if let Some((modified, len)) = identity
            && let Some(hub_url) = self.cached_cover_url(book_id, modified, len)
        {
            return Ok(CoverUpload::AlreadySent(hub_url));
        }

        // `books.cover_url` replicates but the file does not: the cover lane
        // carries it separately (ADR-046), so between the row landing on a
        // device and its bytes arriving, the derived path resolves to nothing.
        // That is an absence, not a failure, and the two must not share an exit.
        //
        // Only in app mode: there the path is derived from the book's identity,
        // so nothing being there means exactly "the bytes have not arrived".
        // The server binary has no covers directory and no cover lane, it reads
        // the stored path as given, and a path that leads nowhere there is a
        // genuine local fault worth reporting.
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && covers_dir.is_some() => {
                return Ok(CoverUpload::Missing);
            }
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };

        let jpeg_bytes = tokio::task::spawn_blocking(move || {
            crate::utils::cover_image::resize_to_jpeg_thumbnail(&bytes)
        })
        .await
        .map_err(|e| format!("spawn: {e}"))??;

        let hub_url = self
            .upload_cover(db, book_id, jpeg_bytes)
            .await
            .map_err(|e| format!("upload: {e}"))?;

        if let Some((modified, len)) = identity {
            self.remember_uploaded_cover(book_id, modified, len, &hub_url);
        }
        Ok(CoverUpload::Sent(hub_url))
    }

    /// End-to-end wrapper around `resize_and_upload_cover` that also drives
    /// the `hub_cover_upload_failed_at` flag: clears it on success, sets it
    /// on failure, so the owner's UI stays in sync with the actual hub state
    /// without the caller having to remember the bookkeeping. Returns the
    /// hub URL on success, `None` on failure (already logged at ERROR).
    ///
    /// A cover whose bytes are not on this device is neither: it clears the
    /// flag and returns `None` without touching the network.
    pub async fn process_local_cover_upload(
        &self,
        db: &DatabaseConnection,
        book_id: &str,
        covers_dir: Option<&std::path::Path>,
        stored_cover: &str,
    ) -> Option<String> {
        match self
            .resize_and_upload_cover(db, book_id, covers_dir, stored_cover)
            .await
        {
            Ok(CoverUpload::Sent(hub_url)) => {
                Self::clear_hub_cover_upload_failure(db, book_id).await;
                Some(hub_url)
            }
            // Nothing was sent, so nothing needs clearing: the upload this hit
            // replays already cleared the flag, and a failure since would have
            // evicted the entry. Saves one DB write per cover on a loop that
            // runs 5s behind every book edit.
            Ok(CoverUpload::AlreadySent(hub_url)) => Some(hub_url),
            // Nothing to upload and nothing to report. Flagging an absence
            // would pin a permanent "cover not synced" badge on every book a
            // paired device catalogued with a photo, on a device that never
            // held the file and so can never clear the flag by succeeding.
            // Clearing is what brings down the flags an earlier build raised
            // when it read this same absence as a failure.
            Ok(CoverUpload::Missing) => {
                tracing::debug!(
                    "cover bytes for book {book_id} are not on this device; \
                     nothing to upload until the cover lane delivers them"
                );
                Self::clear_hub_cover_upload_failure(db, book_id).await;
                None
            }
            Err(e) => {
                tracing::error!("cover upload failed for book {book_id}: {e}");
                // Drop any entry for this book, so the retry really re-uploads
                // instead of replaying a hit and skipping the flag clearing
                // above. Without it the badge raised on the next line would
                // never come down again this run.
                self.forget_uploaded_cover(book_id);
                Self::mark_hub_cover_upload_failure(db, book_id).await;
                None
            }
        }
    }

    /// Fetches the catalog of a public or approved library from the hub.
    ///
    /// Returns enriched entries if available, otherwise falls back to ISBN-only entries.
    pub async fn get_catalog(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
    ) -> Result<Vec<CatalogEntry>, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;
        let cfg = Self::get_config(db).await?;

        let mut req = self
            .http_client
            .get(format!("{hub_url}/api/directory/{node_id}/catalog"));
        if let Some(ref c) = cfg {
            req = req.header("Authorization", format!("Bearer {}", c.write_token));
        }

        let response = req.send().await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        let catalog: HubCatalog = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;

        // Prefer enriched catalog_payload if present
        if let Some(ref cp) = catalog.catalog_payload {
            match parse_catalog_entries(cp) {
                Ok(entries) => return Ok(entries),
                Err(e) => {
                    tracing::warn!(
                        "enriched catalog payload for {node_id} unreadable ({e}); \
                         falling back to the ISBN-only payload"
                    );
                }
            }
        }

        // Fallback: legacy ISBN-only list
        let isbns: Vec<String> = serde_json::from_str(&catalog.isbn_payload)
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;

        Ok(isbns
            .into_iter()
            .map(|isbn| CatalogEntry {
                isbn,
                book_id: None,
                title: String::new(),
                author: None,
                cover_url: None,
                added_at: None,
            })
            .collect())
    }

    // -----------------------------------------------------------------------
    // Directory listing
    // -----------------------------------------------------------------------

    pub async fn list_directory(
        &self,
        limit: i64,
        offset: i64,
        country: Option<&str>,
        search: Option<&str>,
        city_id: Option<i64>,
    ) -> Result<Vec<HubProfile>, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;
        let mut url = format!("{hub_url}/api/directory?limit={limit}&offset={offset}");
        if let Some(c) = country {
            url.push_str(&format!("&country={c}"));
        }
        if let Some(id) = city_id {
            // ADR-035 Phase 2: hub validates city_id is a positive integer,
            // so we forward Some(0) as well even though the picker should
            // never produce it - the hub will reply 400 and the UI can
            // surface that as a generic error.
            url.push_str(&format!("&city_id={id}"));
        }
        if let Some(s) = search {
            url.push_str(&format!("&search={}", urlencoding::encode(s)));
        }

        let response = self.http_client.get(&url).send().await?;
        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        #[derive(Deserialize)]
        struct DirectoryPage {
            items: Vec<HubProfile>,
        }

        let page: DirectoryPage = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;

        Ok(page.items)
    }

    pub async fn get_profile(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
    ) -> Result<HubProfile, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;
        let mut req = self
            .http_client
            .get(format!("{hub_url}/api/directory/{node_id}"));
        // Attach Bearer token so non-listed profiles are accessible
        if let Some(cfg) = Self::get_config(db).await.ok().flatten() {
            req = req.header("Authorization", format!("Bearer {}", cfg.write_token));
        }
        let response = req.send().await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))
    }

    // -----------------------------------------------------------------------
    // Follow lifecycle
    // -----------------------------------------------------------------------

    pub async fn follow(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
        x25519_public_key: Option<&str>,
    ) -> Result<HubFollow, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let mut body = serde_json::Map::new();
        if let Some(key) = x25519_public_key {
            body.insert(
                "x25519_public_key".to_string(),
                serde_json::Value::String(key.to_string()),
            );
        }

        let response = self
            .http_client
            .post(format!("{hub_url}/api/directory/follow/{node_id}"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))
    }

    pub async fn pending_requests(
        &self,
        db: &DatabaseConnection,
    ) -> Result<Vec<HubFollow>, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .get(format!("{hub_url}/api/directory/follows/pending"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        #[derive(Deserialize)]
        struct PendingPage {
            items: Vec<HubFollow>,
        }
        let page: PendingPage = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
        Ok(page.items)
    }

    /// resolution: "approve" | "reject" | "block"
    /// encrypted_contact: optional sealed blob to attach when approving
    pub async fn resolve_follow(
        &self,
        db: &DatabaseConnection,
        follow_id: i64,
        resolution: &str,
        encrypted_contact: Option<&str>,
    ) -> Result<HubFollow, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let mut body = serde_json::json!({ "resolution": resolution });
        if let Some(blob) = encrypted_contact {
            body["encrypted_contact"] = serde_json::Value::String(blob.to_string());
        }

        let response = self
            .http_client
            .patch(format!("{hub_url}/api/directory/follows/{follow_id}"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&body)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))
    }

    pub async fn list_following(
        &self,
        db: &DatabaseConnection,
    ) -> Result<Vec<HubFollow>, HubDirectoryError> {
        self.fetch_follows(db, "following").await
    }

    pub async fn list_followers(
        &self,
        db: &DatabaseConnection,
    ) -> Result<Vec<HubFollow>, HubDirectoryError> {
        self.fetch_follows(db, "followers").await
    }

    pub async fn unfollow(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
    ) -> Result<(), HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .delete(format!("{hub_url}/api/directory/follows/{node_id}"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }
        Ok(())
    }

    /// Batch-updates encrypted contact blobs for all active followers.
    /// Called when the library owner changes their contact info.
    pub async fn sync_follow_contacts(
        &self,
        db: &DatabaseConnection,
        contacts: &[(i64, String)], // (follow_id, encrypted_contact_base64)
    ) -> Result<i32, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let payload: Vec<serde_json::Value> = contacts
            .iter()
            .map(|(id, blob)| {
                serde_json::json!({
                    "follow_id": id,
                    "encrypted_contact": blob,
                })
            })
            .collect();

        let response = self
            .http_client
            .post(format!("{hub_url}/api/directory/contacts/sync"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&serde_json::json!({ "contacts": payload }))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        #[derive(Deserialize)]
        struct SyncResult {
            updated: i32,
        }
        let result: SyncResult = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
        Ok(result.updated)
    }

    /// Completely removes the library profile from the hub directory.
    /// Deletes the profile, all follows (as follower and followed), and cached catalogs.
    pub async fn delete_profile(&self, db: &DatabaseConnection) -> Result<(), HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .delete(format!("{hub_url}/api/directory/profile"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Borrow requests (ADR-018)
    // -----------------------------------------------------------------------

    /// Creates a hub-mediated borrow request.
    pub async fn create_borrow_request(
        &self,
        db: &DatabaseConnection,
        lender_node_id: &str,
        isbn: &str,
        book_title: &str,
    ) -> Result<HubBorrowRequest, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .post(format!("{hub_url}/api/directory/borrow"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&serde_json::json!({
                "lender_node_id": lender_node_id,
                "isbn": isbn,
                "book_title": book_title,
            }))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))
    }

    /// Fetches incoming (pending) borrow requests for the local library as lender.
    pub async fn incoming_borrow_requests(
        &self,
        db: &DatabaseConnection,
    ) -> Result<Vec<HubBorrowRequest>, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .get(format!("{hub_url}/api/directory/borrow/incoming"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        #[derive(Deserialize)]
        struct BorrowPage {
            items: Vec<HubBorrowRequest>,
        }
        let page: BorrowPage = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
        Ok(page.items)
    }

    /// Fetches outgoing borrow requests sent by the local library as requester.
    pub async fn outgoing_borrow_requests(
        &self,
        db: &DatabaseConnection,
    ) -> Result<Vec<HubBorrowRequest>, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .get(format!("{hub_url}/api/directory/borrow/outgoing"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        #[derive(Deserialize)]
        struct BorrowPage {
            items: Vec<HubBorrowRequest>,
        }
        let page: BorrowPage = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
        Ok(page.items)
    }

    /// Resolves a borrow request (accept or reject). Only the lender can resolve.
    pub async fn resolve_borrow_request(
        &self,
        db: &DatabaseConnection,
        request_id: i64,
        resolution: &str,
    ) -> Result<HubBorrowRequest, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .patch(format!("{hub_url}/api/directory/borrow/{request_id}"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .json(&serde_json::json!({ "resolution": resolution }))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))
    }

    /// Cancels a borrow request. Only the requester can cancel.
    pub async fn cancel_borrow_request(
        &self,
        db: &DatabaseConnection,
        request_id: i64,
    ) -> Result<(), HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .delete(format!("{hub_url}/api/directory/borrow/{request_id}"))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn fetch_follows(
        &self,
        db: &DatabaseConnection,
        direction: &str,
    ) -> Result<Vec<HubFollow>, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        let response = self
            .http_client
            .get(format!(
                "{hub_url}/api/directory/follows?direction={direction}"
            ))
            .header("Authorization", format!("Bearer {}", cfg.write_token))
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let msg = response.text().await.unwrap_or_default();
            return Err(HubDirectoryError::Hub(status, msg));
        }

        #[derive(Deserialize)]
        struct FollowPage {
            items: Vec<HubFollow>,
        }
        let page: FollowPage = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
        Ok(page.items)
    }
}

impl Default for HubDirectoryService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod catalog_hash_tests {
    use super::*;

    fn entry(isbn: &str, title: &str, author: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            isbn: isbn.to_string(),
            book_id: None,
            title: title.to_string(),
            author: author.map(str::to_string),
            cover_url: None,
            added_at: None,
        }
    }

    fn payloads(entries: &[CatalogEntry]) -> (String, String) {
        let isbns: Vec<&str> = entries.iter().map(|e| e.isbn.as_str()).collect();
        (
            serde_json::to_string(&isbns).unwrap(),
            serde_json::to_string(entries).unwrap(),
        )
    }

    #[test]
    fn hash_is_64_char_lowercase_hex() {
        let (i, c) = payloads(&[entry("978A", "t", None)]);
        let h = compute_catalog_hash(&i, &c, 1);
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert!(!h.contains('"'));
    }

    #[test]
    fn hash_is_deterministic_for_identical_inputs() {
        let e = vec![entry("9781", "Title", Some("Auth"))];
        let (i, c) = payloads(&e);
        assert_eq!(
            compute_catalog_hash(&i, &c, 42),
            compute_catalog_hash(&i, &c, 42),
        );
    }

    #[test]
    fn hash_differs_when_book_count_changes() {
        let (i, c) = payloads(&[entry("9781", "Title", None)]);
        assert_ne!(
            compute_catalog_hash(&i, &c, 1),
            compute_catalog_hash(&i, &c, 2),
        );
    }

    #[test]
    fn hash_differs_when_catalog_payload_changes() {
        let (i1, c1) = payloads(&[entry("9781", "Old", None)]);
        let (i2, c2) = payloads(&[entry("9781", "New", None)]);
        // ISBN list unchanged, but enriched payload differs.
        assert_eq!(i1, i2);
        assert_ne!(c1, c2);
        assert_ne!(
            compute_catalog_hash(&i1, &c1, 1),
            compute_catalog_hash(&i2, &c2, 1),
        );
    }

    #[test]
    fn hash_differs_when_isbn_payload_changes() {
        let (i1, c1) = payloads(&[entry("9781", "T", None)]);
        let (i2, c2) = payloads(&[entry("9782", "T", None)]);
        assert_ne!(
            compute_catalog_hash(&i1, &c1, 1),
            compute_catalog_hash(&i2, &c2, 1),
        );
    }

    #[test]
    fn hash_is_unambiguous_against_field_boundary_collision() {
        // Without length-prefixing, moving bytes across the isbn/catalog
        // boundary could collide. Length-prefixing prevents that.
        let h1 = compute_catalog_hash("[\"A\"]", "[{\"isbn\":\"B\"}]", 1);
        let h2 = compute_catalog_hash("[\"A\"][{\"isbn\":\"B\"}]", "", 1);
        assert_ne!(h1, h2);
    }

    #[test]
    fn outcome_pushed_differs_from_skipped() {
        assert_ne!(PushCatalogOutcome::Pushed, PushCatalogOutcome::SkippedLocal);
        assert_ne!(
            PushCatalogOutcome::SkippedLocal,
            PushCatalogOutcome::SkippedRemote,
        );
    }

    #[test]
    fn local_skip_allowed_only_after_recent_network_push() {
        let now = chrono::Utc::now();
        let fresh = (now - chrono::Duration::hours(1)).to_rfc3339();
        assert!(hub_catalog_recently_pushed(Some(&fresh), now));

        // Older than the window: the hub's cached-catalog TTL needs a
        // refresh, so the fast path must yield to a real push.
        let stale = (now
            - chrono::Duration::days(CATALOG_LOCAL_SKIP_MAX_AGE_DAYS)
            - chrono::Duration::seconds(1))
        .to_rfc3339();
        assert!(!hub_catalog_recently_pushed(Some(&stale), now));
    }

    #[test]
    fn local_skip_denied_without_push_baseline() {
        let now = chrono::Utc::now();
        // Legacy configs (column added by migration 086) and post-recovery
        // resets have no timestamp: both must re-push once to establish it.
        assert!(!hub_catalog_recently_pushed(None, now));
        assert!(!hub_catalog_recently_pushed(Some("not-a-timestamp"), now));
    }
}

#[cfg(test)]
mod catalog_push_state_tests {
    //! Locks the persistence contract of `record_catalog_push_state`:
    //! a confirmed push stores hash + RFC3339 timestamp, a reset clears both.

    use sea_orm::{ConnectionTrait, Database, DatabaseConnection};

    use super::*;

    /// In-memory DB with the `hub_directory_config` schema as produced by
    /// the base CREATE TABLE plus migrations 055 (allow_borrowing),
    /// 064 (recovery_code), 068 (last_catalog_hash) and
    /// 086 (last_catalog_pushed_at), seeded with the singleton row.
    async fn db_with_config() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "CREATE TABLE hub_directory_config (
                id                     INTEGER PRIMARY KEY DEFAULT 1,
                node_id                TEXT NOT NULL,
                write_token            TEXT NOT NULL,
                is_listed              INTEGER NOT NULL DEFAULT 0,
                requires_approval      INTEGER NOT NULL DEFAULT 1,
                accept_from            TEXT NOT NULL DEFAULT 'everyone',
                allow_borrowing        INTEGER NOT NULL DEFAULT 1,
                recovery_code          TEXT,
                last_catalog_hash      TEXT,
                last_catalog_pushed_at TEXT,
                created_at             TEXT NOT NULL,
                updated_at             TEXT NOT NULL
            )",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO hub_directory_config (id, node_id, write_token, created_at, updated_at)
             VALUES (1, 'test-node', 'test-token', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn confirmed_push_records_hash_and_rfc3339_timestamp() {
        let db = db_with_config().await;

        HubDirectoryService::record_catalog_push_state(&db, Some("abc123"))
            .await
            .unwrap();

        let cfg = HubDirectoryService::get_config(&db).await.unwrap().unwrap();
        assert_eq!(cfg.last_catalog_hash.as_deref(), Some("abc123"));
        let pushed_at = cfg.last_catalog_pushed_at.expect("timestamp recorded");
        assert!(chrono::DateTime::parse_from_rfc3339(&pushed_at).is_ok());
        // A just-recorded push must satisfy the fast-path freshness check.
        assert!(hub_catalog_recently_pushed(
            Some(&pushed_at),
            chrono::Utc::now(),
        ));
    }

    #[tokio::test]
    async fn reset_clears_hash_and_timestamp() {
        let db = db_with_config().await;
        HubDirectoryService::record_catalog_push_state(&db, Some("abc123"))
            .await
            .unwrap();

        HubDirectoryService::record_catalog_push_state(&db, None)
            .await
            .unwrap();

        let cfg = HubDirectoryService::get_config(&db).await.unwrap().unwrap();
        assert_eq!(cfg.last_catalog_hash, None);
        assert_eq!(cfg.last_catalog_pushed_at, None);
    }
}

#[cfg(test)]
mod register_body_tests {
    //! Locks the hub upsert body contract for ADR-035 §8: clearing the city
    //! must propagate to the hub. Other Option fields keep the historical
    //! "Some = set, None = leave alone" semantics.

    use super::*;

    fn base_params() -> RegisterParams {
        RegisterParams {
            node_id: "node123".to_string(),
            display_name: "Test".to_string(),
            is_listed: true,
            requires_approval: false,
            accept_from: "anyone".to_string(),
            allow_borrowing: true,
            ..Default::default()
        }
    }

    #[test]
    fn body_includes_city_id_when_some() {
        let params = RegisterParams {
            location_city_id: Some(2_988_507),
            ..base_params()
        };
        let body = HubDirectoryService::build_register_body(&params, 0);
        assert_eq!(body["location_city_id"], serde_json::json!(2_988_507));
    }

    #[test]
    fn body_sends_null_city_id_when_none() {
        // ADR-035 §8: None must serialize as JSON null so the hub clears
        // the stored value (its upsert uses array_key_exists to detect
        // the field). Omitting the key would silently leave the previous
        // value in place after the user toggles off "Partager ma ville".
        let params = RegisterParams {
            location_city_id: None,
            ..base_params()
        };
        let body = HubDirectoryService::build_register_body(&params, 0);
        assert!(body.get("location_city_id").is_some());
        assert_eq!(body["location_city_id"], serde_json::Value::Null);
    }

    #[test]
    fn body_omits_country_when_none() {
        // Counter-test: the city carve-out must NOT spread to other
        // optional fields. location_country still follows the legacy
        // "absent key = preserve" contract; only city_id is special.
        let params = RegisterParams {
            location_country: None,
            ..base_params()
        };
        let body = HubDirectoryService::build_register_body(&params, 0);
        assert!(body.get("location_country").is_none());
    }
}

#[cfg(test)]
mod catalog_parse_tests {
    use super::parse_catalog_entries;

    #[test]
    fn numeric_book_id_from_pre_uuid_builds_decodes() {
        let payload = r#"[
            {"isbn":"9782020086929","book_id":42,"title":"La Peste","author":"Camus"},
            {"isbn":"2221001885","book_id":"0197f2a4","title":"Dolto","author":null}
        ]"#;
        let entries = parse_catalog_entries(payload).expect("array parses");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].book_id.as_deref(), Some("42"));
        assert_eq!(entries[0].title, "La Peste");
        assert_eq!(entries[1].book_id.as_deref(), Some("0197f2a4"));
    }

    #[test]
    fn malformed_entry_degrades_alone_not_the_whole_catalog() {
        // First entry has a null title (undecodable), second is healthy. The
        // old all-or-nothing decode blanked BOTH; now only the broken one
        // degrades to ISBN-only.
        let payload = r#"[
            {"isbn":"2221001885","title":null},
            {"isbn":"9782020086929","title":"La Peste","author":"Camus"}
        ]"#;
        let entries = parse_catalog_entries(payload).expect("array parses");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].isbn, "2221001885");
        assert_eq!(entries[0].title, "");
        assert_eq!(entries[1].title, "La Peste");
    }

    #[test]
    fn numeric_isbn_is_recovered_on_degraded_entries() {
        let payload = r#"[{"isbn":2221001885,"title":"X"}]"#;
        let entries = parse_catalog_entries(payload).expect("array parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].isbn, "2221001885");
        assert_eq!(entries[0].title, "");
    }

    #[test]
    fn entry_without_recoverable_isbn_is_dropped() {
        let payload =
            r#"[{"title":null},{"isbn":"9782020086929","title":"La Peste","author":null}]"#;
        let entries = parse_catalog_entries(payload).expect("array parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "La Peste");
    }

    #[test]
    fn non_array_payload_errors() {
        assert!(parse_catalog_entries("{\"oops\":1}").is_err());
        assert!(parse_catalog_entries("not json").is_err());
    }

    #[test]
    fn book_uuid_wire_name_is_read_and_written() {
        // Current builds write `book_uuid` (old readers ignore the unknown
        // key instead of failing on a string `book_id`) and read both names.
        let payload = r#"[{
            "isbn":"9782020086929",
            "book_uuid":"0197f2a4-1111-7222-8333-444455556666",
            "title":"La Peste","author":null
        }]"#;
        let entries = parse_catalog_entries(payload).expect("array parses");
        assert_eq!(
            entries[0].book_id.as_deref(),
            Some("0197f2a4-1111-7222-8333-444455556666")
        );

        let json = serde_json::to_string(&entries[0]).expect("serializes");
        assert!(json.contains("\"book_uuid\""), "writes book_uuid: {json}");
        assert!(
            !json.contains("\"book_id\""),
            "never writes book_id: {json}"
        );
    }
}
