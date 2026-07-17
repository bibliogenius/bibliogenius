// Hub directory service plumbing, DTOs, config, registration, recovery.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// =============================================================================
// Hub Directory (ADR-015)
// =============================================================================

use crate::services::hub_directory_service::{
    CatalogEntry, DirectoryConfig, HubBorrowRequest, HubDirectoryError, HubDirectoryService,
    HubFollow, HubProfile, RegisterParams,
};

static HUB_DIRECTORY_SVC: OnceLock<HubDirectoryService> = OnceLock::new();

fn hub_directory_svc() -> &'static HubDirectoryService {
    HUB_DIRECTORY_SVC.get_or_init(HubDirectoryService::new)
}

fn hub_db() -> Result<&'static sea_orm::DatabaseConnection, String> {
    db().ok_or_else(|| "Database not initialized".to_string())
}

// ---------------------------------------------------------------------------
// FFI structs
// ---------------------------------------------------------------------------

#[frb(dart_metadata=("freezed"))]
pub struct FrbDirectoryConfig {
    pub node_id: String,
    pub is_listed: bool,
    pub requires_approval: bool,
    pub accept_from: String,
    pub allow_borrowing: bool,
}

impl From<DirectoryConfig> for FrbDirectoryConfig {
    fn from(c: DirectoryConfig) -> Self {
        Self {
            node_id: c.node_id,
            is_listed: c.is_listed,
            requires_approval: c.requires_approval,
            accept_from: c.accept_from,
            allow_borrowing: c.allow_borrowing,
        }
    }
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbHubProfile {
    pub node_id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub book_count: i32,
    pub location_country: Option<String>,
    pub location_city_id: Option<i64>,
    pub requires_approval: bool,
    pub allow_borrowing: Option<bool>,
    pub last_seen_at: Option<String>,
    pub x25519_public_key: Option<String>,
    pub website: Option<String>,
    pub device_model: Option<String>,
    pub device_fingerprint: Option<String>,
    pub app_version: Option<String>,
    pub avatar_config: Option<String>,
}

impl From<HubProfile> for FrbHubProfile {
    fn from(p: HubProfile) -> Self {
        Self {
            node_id: p.node_id,
            display_name: p.display_name,
            description: p.description,
            book_count: p.book_count,
            location_country: p.location_country,
            location_city_id: p.location_city_id,
            requires_approval: p.requires_approval,
            allow_borrowing: p.allow_borrowing,
            last_seen_at: p.last_seen_at,
            x25519_public_key: p.x25519_public_key,
            website: p.website,
            device_model: p.device_model,
            device_fingerprint: p.device_fingerprint,
            app_version: p.app_version,
            avatar_config: p.avatar_config,
        }
    }
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbRegisterParams {
    pub node_id: String,
    pub display_name: String,
    pub book_count: i32,
    pub is_listed: bool,
    pub requires_approval: bool,
    pub accept_from: String,
    pub description: Option<String>,
    pub location_country: Option<String>,
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

impl From<FrbRegisterParams> for RegisterParams {
    fn from(p: FrbRegisterParams) -> Self {
        Self {
            node_id: p.node_id,
            display_name: p.display_name,
            book_count: p.book_count,
            is_listed: p.is_listed,
            requires_approval: p.requires_approval,
            accept_from: p.accept_from,
            description: p.description,
            location_country: p.location_country,
            location_city_id: p.location_city_id,
            allow_borrowing: p.allow_borrowing,
            x25519_public_key: p.x25519_public_key,
            website: p.website,
            device_model: p.device_model,
            device_fingerprint: p.device_fingerprint,
            app_version: p.app_version,
            relay_url: p.relay_url,
            relay_mailbox_id: p.relay_mailbox_id,
            relay_write_token: p.relay_write_token,
            avatar_config: p.avatar_config,
        }
    }
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbHubFollow {
    pub id: i64,
    pub follower_node_id: String,
    pub followed_node_id: String,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub follower_display_name: Option<String>,
    pub encrypted_contact: Option<String>,
    pub follower_x25519_public_key: Option<String>,
}

impl From<HubFollow> for FrbHubFollow {
    fn from(f: HubFollow) -> Self {
        Self {
            id: f.id,
            follower_node_id: f.follower_node_id,
            followed_node_id: f.followed_node_id,
            status: f.status,
            created_at: f.created_at,
            resolved_at: f.resolved_at,
            follower_display_name: f.follower_display_name,
            encrypted_contact: f.encrypted_contact,
            follower_x25519_public_key: f.follower_x25519_public_key,
        }
    }
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbCatalogEntry {
    pub isbn: String,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    /// Owner's `books.created_at` broadcast through the catalog payload.
    /// Source of truth for the "NEW" badge: every viewer agrees on what's
    /// recent because the timestamp lives on the owner's side.
    pub added_at: Option<String>,
}

impl From<CatalogEntry> for FrbCatalogEntry {
    fn from(e: CatalogEntry) -> Self {
        Self {
            isbn: e.isbn,
            title: e.title,
            author: e.author,
            cover_url: e.cover_url,
            added_at: e.added_at,
        }
    }
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbHubBorrowRequest {
    pub id: i64,
    pub requester_node_id: String,
    pub lender_node_id: String,
    pub isbn: String,
    pub book_title: String,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub requester_display_name: Option<String>,
    pub lender_display_name: Option<String>,
}

impl From<HubBorrowRequest> for FrbHubBorrowRequest {
    fn from(r: HubBorrowRequest) -> Self {
        Self {
            id: r.id,
            requester_node_id: r.requester_node_id,
            lender_node_id: r.lender_node_id,
            isbn: r.isbn,
            book_title: r.book_title,
            status: r.status,
            created_at: r.created_at,
            resolved_at: r.resolved_at,
            requester_display_name: r.requester_display_name,
            lender_display_name: r.lender_display_name,
        }
    }
}

// ---------------------------------------------------------------------------
// FFI functions
// ---------------------------------------------------------------------------

/// Returns the local hub directory settings, or None if not yet registered.
pub async fn hub_directory_get_config() -> Result<Option<FrbDirectoryConfig>, String> {
    let db = hub_db()?;
    HubDirectoryService::get_config(db)
        .await
        .map(|opt| opt.map(FrbDirectoryConfig::from))
        .map_err(|e| e.to_string())
}

/// Returns the local relay configuration (relay_url, mailbox_uuid, write_token).
/// Returns None if relay is not configured yet.
/// Note: read_token is intentionally excluded (S2: never leaves the device).
pub async fn get_relay_config_ffi() -> Result<Option<FrbRelayConfig>, String> {
    let db = hub_db()?;
    let config = crate::api::relay::get_my_relay_config(db).await;
    Ok(config.map(|c| FrbRelayConfig {
        relay_url: c.relay_url,
        mailbox_uuid: c.mailbox_uuid,
        write_token: c.write_token,
    }))
}

/// Relay config exposed via FFI. Excludes read_token (S2).
#[frb(dart_metadata=("freezed"))]
pub struct FrbRelayConfig {
    pub relay_url: String,
    pub mailbox_uuid: String,
    pub write_token: String,
}

/// Exports the hub directory write_token for Keychain backup.
/// Used by Flutter to persist the token in platform-secure storage
/// so it survives app reinstalls (critical on iOS).
/// Returns None if not yet registered.
pub async fn hub_directory_export_write_token() -> Result<Option<String>, String> {
    let db = hub_db()?;
    HubDirectoryService::get_write_token(db)
        .await
        .map_err(|e| e.to_string())
}

/// Imports a write_token recovered from Keychain after app reinstall.
/// Restores hub authentication without requiring a new registration.
pub async fn hub_directory_import_write_token(
    node_id: String,
    write_token: String,
) -> Result<(), String> {
    let db = hub_db()?;
    HubDirectoryService::import_write_token(db, &node_id, &write_token)
        .await
        .map_err(|e| e.to_string())
}

/// Purges the local hub_directory_config row, forcing a fresh registration
/// on the next ensureRegistered() call. Used for 401 recovery when the
/// stored write_token is no longer valid on the hub.
pub async fn hub_directory_purge_config() -> Result<(), String> {
    let db = hub_db()?;
    use sea_orm::ConnectionTrait;
    db.execute(sea_orm::Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM hub_directory_config".to_owned(),
    ))
    .await
    .map_err(|e| format!("Failed to purge hub_directory_config: {e}"))?;
    // Clear any pending cover-upload failures: once unregistered, the old
    // warnings become meaningless and would never auto-clear (no next sync
    // with the old registration). Next registration starts fresh.
    crate::services::hub_directory_service::HubDirectoryService::reset_all_hub_cover_upload_failures(db).await;
    tracing::info!("hub_directory_config purged for 401 recovery");
    Ok(())
}

/// Returns the locally stored recovery code for display in settings.
/// Returns None if not yet registered or if registration predates recovery codes.
pub async fn hub_directory_get_recovery_code() -> Result<Option<String>, String> {
    let db = hub_db()?;
    HubDirectoryService::get_recovery_code(db)
        .await
        .map_err(|e| e.to_string())
}

/// Recovers a hub profile using a one-time recovery code.
/// On success: stores the new write_token + recovery_code locally and returns the config.
pub async fn hub_directory_recover(
    node_id: String,
    recovery_code: String,
) -> Result<FrbDirectoryConfig, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .recover(db, &node_id, &recovery_code)
        .await
        .map(FrbDirectoryConfig::from)
        .map_err(|e| e.to_string())
}

/// Registers with the hub directory (first call) or updates the profile.
/// On first registration, the write_token is persisted automatically.
pub async fn hub_directory_register(
    params: FrbRegisterParams,
) -> Result<FrbDirectoryConfig, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .register_or_update(db, params.into())
        .await
        .map(FrbDirectoryConfig::from)
        .map_err(|e| e.to_string())
}
