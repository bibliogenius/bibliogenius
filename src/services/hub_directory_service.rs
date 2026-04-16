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
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
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
/// Books without ISBN use `book_id` as an alternative key.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CatalogEntry {
    pub isbn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_id: Option<i32>,
    pub title: String,
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
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

#[derive(Debug, Clone, Default)]
pub struct RegisterParams {
    pub node_id: String,
    pub display_name: String,
    pub book_count: i32,
    pub is_listed: bool,
    pub requires_approval: bool,
    pub accept_from: String,
    pub description: Option<String>,
    pub location_country: Option<String>,
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
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct HubDirectoryService {
    http_client: Client,
}

impl HubDirectoryService {
    pub fn new() -> Self {
        let http_client = Client::builder()
            .user_agent("BiblioGenius/1.0")
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self { http_client }
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
                "SELECT node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, recovery_code, last_catalog_hash
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

    /// Persist the hash of the last catalog push so the next sync can
    /// skip the HTTP round-trip when the catalog is unchanged.
    ///
    /// Passing `None` resets the hash, which forces the next sync to
    /// re-push unconditionally (used after recovery where the hub's
    /// cached catalog may have been lost).
    pub(crate) async fn update_last_catalog_hash(
        db: &DatabaseConnection,
        hash: Option<&str>,
    ) -> Result<(), HubDirectoryError> {
        let backend = db.get_database_backend();
        let value = match hash {
            Some(h) => format!("'{}'", h.replace('\'', "''")),
            None => "NULL".to_string(),
        };
        db.execute(Statement::from_string(
            backend,
            format!(
                "UPDATE hub_directory_config
                 SET last_catalog_hash = {value},
                     updated_at = '{now}'
                 WHERE id = 1",
                now = chrono::Utc::now().to_rfc3339()
            ),
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

    /// Registers the library with the hub directory (first call) or updates its profile.
    /// On first registration, the hub returns a write_token that is persisted locally.
    pub async fn register_or_update(
        &self,
        db: &DatabaseConnection,
        params: RegisterParams,
    ) -> Result<DirectoryConfig, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;
        let existing = Self::get_config(db).await?;

        let mut body = serde_json::json!({
            "node_id":           params.node_id,
            "display_name":      params.display_name,
            "book_count":        params.book_count,
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

        let initial_token = existing.as_ref().map(|c| c.write_token.clone());
        let has_auth = initial_token.is_some();

        tracing::info!(
            "Hub directory: register_or_update node_id={} hub={} auth={} relay_mailbox={}",
            &params.node_id[..12.min(params.node_id.len())],
            hub_url,
            has_auth,
            params.relay_mailbox_id.as_deref().unwrap_or("none"),
        );

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

        let config = DirectoryConfig {
            node_id: params.node_id,
            write_token,
            is_listed: params.is_listed,
            requires_approval: params.requires_approval,
            accept_from: params.accept_from,
            allow_borrowing: params.allow_borrowing,
            recovery_code,
            last_catalog_hash,
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
        };

        Self::save_config(db, &config).await?;
        // save_config preserves existing columns that aren't in its SET list;
        // last_catalog_hash is one of them. Force a reset here so the next
        // sync re-pushes (hub's CachedCatalog may have been lost/expired).
        Self::update_last_catalog_hash(db, None).await?;
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
        if cfg.last_catalog_hash.as_deref() == Some(catalog_hash.as_str()) {
            tracing::debug!(
                target: "hub_directory",
                "push_catalog: skipped (local hash match)"
            );
            return Ok(PushCatalogOutcome::SkippedLocal);
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
            Self::update_last_catalog_hash(db, Some(&catalog_hash)).await?;
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
        Self::update_last_catalog_hash(db, Some(&catalog_hash)).await?;
        Ok(PushCatalogOutcome::Pushed)
    }

    /// Uploads a cover thumbnail to the hub.
    ///
    /// Returns the public URL where the cover can be fetched.
    pub async fn upload_cover(
        &self,
        db: &DatabaseConnection,
        book_id: i32,
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
        if let Some(ref cp) = catalog.catalog_payload
            && let Ok(entries) = serde_json::from_str::<Vec<CatalogEntry>>(cp)
        {
            return Ok(entries);
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
    ) -> Result<Vec<HubProfile>, HubDirectoryError> {
        let hub_url = Self::hub_base_url()?;
        let mut url = format!("{hub_url}/api/directory?limit={limit}&offset={offset}");
        if let Some(c) = country {
            url.push_str(&format!("&country={c}"));
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
}
