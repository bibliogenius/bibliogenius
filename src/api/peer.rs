#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
use crate::models::{operation_log, peer, peer_gamification_stats};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use futures::future::join_all;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info};
use url::Url;

/// Validate URL to prevent SSRF
/// Blocks:
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-Local (169.254.0.0/16, fe80::/10)
/// - AWS Metadata Service (169.254.169.254)
/// - "localhost" hostname
pub fn validate_url(url_str: &str) -> Result<String, String> {
    let url = Url::parse(url_str).map_err(|_| "Invalid URL format".to_string())?;

    // 1. Check Scheme
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Only HTTP/HTTPS schemes allowed".to_string());
    }

    // 2. Check Host
    match url.host() {
        Some(url::Host::Domain("localhost")) => {
            return Err("Localhost access is blocked".to_string());
        }
        Some(url::Host::Ipv4(ip)) if ip.is_loopback() => {
            return Err("Loopback addresses blocked".to_string());
        }
        Some(url::Host::Ipv6(ip)) if ip.is_loopback() => {
            return Err("Loopback addresses blocked".to_string());
        }
        _ => {}
    }

    Ok(url.to_string())
}

/// Create a safe HTTP client with restricted redirects and timeouts
pub(crate) fn get_safe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()) // Disable redirects to prevent bypass
        .build()
        .unwrap_or_default()
}

/// Translate localhost URLs to Docker service names for inter-container communication
/// Examples:
/// - http://localhost:8001 -> http://bibliogenius-a:8000
/// - http://localhost:8002 -> http://bibliogenius-b:8000
fn translate_url_for_docker(url: &str) -> String {
    if url.contains("localhost:8001") {
        url.replace("localhost:8001", "bibliogenius-a:8000")
    } else if url.contains("localhost:8002") {
        url.replace("localhost:8002", "bibliogenius-b:8000")
    } else {
        url.to_string()
    }
}

/// Check if the `connection_validation` module is enabled in installation profile
async fn is_connection_validation_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile;

    if let Ok(Some(profile)) = installation_profile::Entity::find().one(db).await {
        return profile.enabled_modules.contains("connection_validation");
    }
    false
}

/// Check if `auto_approve_loans` module is enabled in installation profile
async fn is_auto_approve_loans_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile;

    if let Ok(Some(profile)) = installation_profile::Entity::find().one(db).await {
        return profile.enabled_modules.contains("auto_approve_loans");
    }
    false
}

/// Check if a specific peer is approved for access.
/// Returns true if connection_validation is disabled OR if the peer has connection_status == "accepted".
async fn is_peer_approved(db: &DatabaseConnection, peer: &peer::Model) -> bool {
    if !is_connection_validation_enabled(db).await {
        return true;
    }
    peer.connection_status == "accepted"
}

/// Try to send a message to a peer via E2EE. Returns Ok(Some(response)) if E2EE succeeded,
/// Ok(None) if E2EE is not available for this peer (caller should fall back to plaintext).
async fn try_send_e2ee(
    state: &crate::infrastructure::AppState,
    peer: &peer::Model,
    message_type: &str,
    payload: serde_json::Value,
) -> Result<Option<Option<crate::crypto::envelope::ClearMessage>>, String> {
    // Check if peer supports E2EE
    if !peer.key_exchange_done {
        tracing::warn!(
            "E2EE: Skipping — peer {} key_exchange_done=false",
            peer.name
        );
        return Ok(None); // Plaintext fallback
    }

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            tracing::warn!("E2EE: Skipping — CryptoService not initialized");
            return Ok(None); // Identity not ready, fallback
        }
    };

    // Parse peer's X25519 public key
    let x25519_hex = match &peer.x25519_public_key {
        Some(hex) => hex,
        None => {
            tracing::warn!(
                "E2EE: Skipping — peer {} missing x25519_public_key",
                peer.name
            );
            return Ok(None);
        }
    };
    let x_bytes = hex::decode(x25519_hex).map_err(|e| format!("Invalid x25519 key: {e}"))?;
    if x_bytes.len() != 32 {
        return Ok(None);
    }
    let x_arr: [u8; 32] = x_bytes.try_into().unwrap();
    let peer_x25519 = x25519_dalek::PublicKey::from(x_arr);

    // Parse peer's Ed25519 verifying key (for opening responses)
    let ed_hex = match &peer.public_key {
        Some(hex) => hex,
        None => return Ok(None),
    };
    let ed_bytes = hex::decode(ed_hex).map_err(|e| format!("Invalid ed25519 key: {e}"))?;
    if ed_bytes.len() != 32 {
        return Ok(None);
    }
    let ed_arr: [u8; 32] = ed_bytes.try_into().unwrap();
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&ed_arr)
        .map_err(|e| format!("Invalid ed25519 key: {e}"))?;

    let peer_info = crate::services::crypto_service::PeerInfo {
        verifying_key,
        x25519_public: peer_x25519,
    };

    let transport = crate::services::e2ee_transport::DirectTransport::new(crypto_service.clone());
    let message =
        crate::services::e2ee_transport::DirectTransport::build_message(message_type, payload);

    match transport
        .send(&peer.url, &peer_x25519, &peer_info, &message)
        .await
    {
        Ok(response) => {
            tracing::info!(
                "E2EE: Sent '{}' to peer {} ({})",
                message_type,
                peer.name,
                peer.id
            );
            Ok(Some(response))
        }
        Err(crate::services::e2ee_transport::E2eeTransportError::Network(ref net_err)) => {
            // Network error — peer unreachable. Try relay fallback for fire-and-forget messages.
            // Request-response messages (search_request, book_sync_request) are NOT relayed
            // because the relay model doesn't support synchronous responses.
            let is_fire_and_forget =
                !matches!(message_type, "search_request" | "book_sync_request");

            if is_fire_and_forget
                && let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
                    (&peer.relay_url, &peer.mailbox_id, &peer.relay_write_token)
            {
                tracing::info!(
                    "E2EE: Direct failed ({}), trying relay for '{}' to peer {}",
                    net_err,
                    message_type,
                    peer.name
                );

                let relay = crate::services::relay_transport::RelayTransport::new(crypto_service);
                match relay
                    .send(relay_url, mailbox_id, write_token, &peer_x25519, &message)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            "E2EE Relay: Sent '{}' to peer {} via relay",
                            message_type,
                            peer.name
                        );
                        // Relay is fire-and-forget: no response
                        return Ok(Some(None));
                    }
                    Err(relay_err) => {
                        tracing::warn!(
                            "E2EE Relay: Also failed for peer {}: {relay_err}",
                            peer.name
                        );
                        return Err(format!(
                            "E2EE send failed (direct: {net_err}, relay: {relay_err})"
                        ));
                    }
                }
            }

            Err(format!("E2EE send failed: network error: {net_err}"))
        }
        Err(e) => Err(format!("E2EE send failed: {e}")),
    }
}

/// Bulk-approve all pending peers (called when connection_validation is toggled OFF)
pub async fn auto_approve_all_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let peers = peer::Entity::find()
        .filter(peer::Column::ConnectionStatus.eq("pending"))
        .all(&db)
        .await
        .unwrap_or_default();

    let count = peers.len();
    for p in peers {
        let mut active: peer::ActiveModel = p.into();
        active.connection_status = Set("accepted".to_string());
        active.auto_approve = Set(true);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active.update(&db).await;
    }

    tracing::info!("✅ Auto-approved {} pending peers", count);
    (
        StatusCode::OK,
        Json(json!({ "message": format!("Approved {} peers", count), "count": count })),
    )
        .into_response()
}

// ── Relay setup ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetupRelayRequest {
    pub relay_url: String,
}

/// POST /api/peers/relay/setup — Register a mailbox on a relay hub.
///
/// Calls the relay hub to create a new mailbox, then stores the config locally.
pub async fn setup_relay(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<SetupRelayRequest>,
) -> impl IntoResponse {
    use crate::models::relay_config;

    let db = state.db();

    // 1. Validate relay URL
    if let Err(e) = validate_url(&payload.relay_url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Call relay hub to create a mailbox
    let client = get_safe_client();
    let url = format!(
        "{}/api/relay/mailbox",
        payload.relay_url.trim_end_matches('/')
    );

    let response = match client.post(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Failed to reach relay hub: {e}") })),
            )
                .into_response();
        }
    };

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Relay hub returned error: {body}") })),
        )
            .into_response();
    }

    let result: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Invalid relay response: {e}") })),
            )
                .into_response();
        }
    };

    let mailbox_uuid = result
        .get("uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let read_token = result
        .get("read_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let write_token = result
        .get("write_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if mailbox_uuid.is_empty() || read_token.is_empty() || write_token.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Relay hub returned incomplete mailbox data" })),
        )
            .into_response();
    }

    // 3. Store in my_relay_config (upsert singleton row)
    use sea_orm::ConnectionTrait;
    let now = chrono::Utc::now().to_rfc3339();

    // Delete existing config if any, then insert
    let _ = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM my_relay_config".to_owned(),
        ))
        .await;

    let config = relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(payload.relay_url),
        mailbox_uuid: Set(mailbox_uuid.clone()),
        read_token: Set(read_token),
        write_token: Set(write_token.clone()),
        created_at: Set(now),
    };

    match config.insert(db).await {
        Ok(_) => {
            tracing::info!("Relay: Mailbox registered: {mailbox_uuid}");
            (
                StatusCode::OK,
                Json(json!({
                    "mailbox_uuid": mailbox_uuid,
                    "write_token": write_token,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save relay config: {e}") })),
        )
            .into_response(),
    }
}

/// GET /api/peers/relay/config — Get current relay config (if any).
pub async fn get_relay_config_endpoint(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    match crate::api::relay::get_my_relay_config(db).await {
        Some(config) => (
            StatusCode::OK,
            Json(json!({
                "relay_url": config.relay_url,
                "mailbox_uuid": config.mailbox_uuid,
                "write_token": config.write_token,
                "created_at": config.created_at,
            })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No relay configured" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct ConnectRequest {
    name: String,
    url: String,
    public_key: Option<String>,
    /// Ed25519 public key (hex) from the remote peer — for E2EE
    #[serde(default)]
    ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the remote peer — for E2EE
    #[serde(default)]
    x25519_public_key: Option<String>,
    /// Peer's relay hub URL
    #[serde(default)]
    relay_url: Option<String>,
    /// Peer's relay mailbox UUID
    #[serde(default)]
    mailbox_id: Option<String>,
    /// Token to write to peer's relay mailbox
    #[serde(default)]
    relay_write_token: Option<String>,
}

pub async fn connect(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<ConnectRequest>,
) -> impl IntoResponse {
    // 1. Validate URL
    if let Err(e) = validate_url(&payload.url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Fetch remote config to get location and verify connectivity
    let client = get_safe_client();
    let config_url = format!("{}/api/config", payload.url.trim_end_matches('/'));

    // Struct to hold remote config data including E2EE keys and relay info
    struct RemoteConfigData {
        latitude: Option<f64>,
        longitude: Option<f64>,
        remote_name: Option<String>,
        ed25519_public_key: Option<String>,
        x25519_public_key: Option<String>,
        relay_url: Option<String>,
        mailbox_id: Option<String>,
        relay_write_token: Option<String>,
    }

    let remote_data = match client.get(&config_url).send().await {
        Ok(res) => {
            if res.status().is_success() {
                match res.json::<crate::api::setup::ConfigResponse>().await {
                    Ok(config) => {
                        let (lat, long) = if config.share_location {
                            (config.latitude, config.longitude)
                        } else {
                            (None, None)
                        };
                        RemoteConfigData {
                            latitude: lat,
                            longitude: long,
                            remote_name: Some(config.library_name),
                            ed25519_public_key: config.ed25519_public_key,
                            x25519_public_key: config.x25519_public_key,
                            relay_url: config.relay_url,
                            mailbox_id: config.mailbox_id,
                            relay_write_token: config.relay_write_token,
                        }
                    }
                    _ => RemoteConfigData {
                        latitude: None,
                        longitude: None,
                        remote_name: None,
                        ed25519_public_key: None,
                        x25519_public_key: None,
                        relay_url: None,
                        mailbox_id: None,
                        relay_write_token: None,
                    },
                }
            } else {
                RemoteConfigData {
                    latitude: None,
                    longitude: None,
                    remote_name: None,
                    ed25519_public_key: None,
                    x25519_public_key: None,
                    relay_url: None,
                    mailbox_id: None,
                    relay_write_token: None,
                }
            }
        }
        Err(_) => RemoteConfigData {
            latitude: None,
            longitude: None,
            remote_name: None,
            ed25519_public_key: None,
            x25519_public_key: None,
            relay_url: None,
            mailbox_id: None,
            relay_write_token: None,
        },
    };

    // Use provided name or fallback to remote name or "Unknown"
    let name = if !payload.name.is_empty() {
        payload.name
    } else {
        remote_data
            .remote_name
            .unwrap_or_else(|| "Unknown Library".to_string())
    };

    // Prefer keys from the request payload (QR/invite), fall back to ConfigResponse keys.
    // Legacy `public_key` field is used as fallback for ed25519 (backward compat).
    let ed25519_key = payload
        .ed25519_public_key
        .or(remote_data.ed25519_public_key)
        .or(payload.public_key);
    let x25519_key = payload.x25519_public_key.or(remote_data.x25519_public_key);

    // Key exchange is done if we have both keys
    let key_exchange_done = ed25519_key.is_some() && x25519_key.is_some();

    // Translate localhost URLs to Docker service names for inter-container communication
    let docker_url = translate_url_for_docker(&payload.url);
    let peer_url_for_sync = docker_url.clone(); // Clone before moving into ActiveModel

    // Upsert: update existing peer by URL, or insert new
    let existing = peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await;

    // Relay info: prefer payload, fall back to remote config
    let relay_url = payload.relay_url.or(remote_data.relay_url);
    let mailbox_id = payload.mailbox_id.or(remote_data.mailbox_id);
    let relay_write_token = payload.relay_write_token.or(remote_data.relay_write_token);

    let peer_id = match existing {
        Ok(Some(existing_peer)) => {
            // Update existing peer with new keys and info
            let peer_id = existing_peer.id;
            let mut active: peer::ActiveModel = existing_peer.into();
            active.name = Set(name);
            active.public_key = Set(ed25519_key.clone());
            active.x25519_public_key = Set(x25519_key);
            active.key_exchange_done = Set(key_exchange_done);
            active.latitude = Set(remote_data.latitude);
            active.longitude = Set(remote_data.longitude);
            active.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            active.auto_approve = Set(true);
            active.connection_status = Set("accepted".to_string());
            // Store relay info if provided
            if relay_url.is_some() {
                active.relay_url = Set(relay_url);
            }
            if mailbox_id.is_some() {
                active.mailbox_id = Set(mailbox_id);
            }
            if relay_write_token.is_some() {
                active.relay_write_token = Set(relay_write_token);
            }
            match active.update(&db).await {
                Ok(_) => peer_id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            }
        }
        _ => {
            // Insert new peer
            let peer = peer::ActiveModel {
                name: Set(name),
                url: Set(docker_url),
                public_key: Set(ed25519_key.clone()),
                x25519_public_key: Set(x25519_key),
                key_exchange_done: Set(key_exchange_done),
                latitude: Set(remote_data.latitude),
                longitude: Set(remote_data.longitude),
                relay_url: Set(relay_url),
                mailbox_id: Set(mailbox_id),
                relay_write_token: Set(relay_write_token),
                last_seen: Set(Some(chrono::Utc::now().to_rfc3339())),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                auto_approve: Set(true),
                ..Default::default()
            };
            match peer::Entity::insert(peer).exec(&db).await {
                Ok(res) => res.last_insert_id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            }
        }
    };

    // Trigger background sync of peer catalog
    let db_clone = db.clone();
    tokio::spawn(async move {
        tracing::info!("🔄 Background sync triggered for new peer {}", peer_id);
        if let Err(e) = sync_peer_internal(&db_clone, peer_id, &peer_url_for_sync).await {
            tracing::warn!("⚠️ Background sync failed for peer {}: {}", peer_id, e);
        }
    });

    (StatusCode::CREATED, Json(json!({ "id": peer_id }))).into_response()
}

/// Internal sync function for background sync after connect
async fn sync_peer_internal(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
) -> Result<usize, String> {
    use crate::models::peer_book;

    // Validate URL
    validate_url(peer_url).map_err(|e| format!("Invalid peer URL: {}", e))?;

    let client = get_safe_client();

    // First, check peer's config for privacy consent flags
    let config_url = format!("{}/api/config", peer_url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };

    let allows_caching = peer_config
        .as_ref()
        .is_some_and(|c| c.allow_library_caching);
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);

    // Extract updated name from peer config (if changed)
    let updated_name = if let Some(config) = &peer_config {
        if let Ok(Some(p)) = peer::Entity::find_by_id(peer_id).one(db).await {
            if p.name != config.library_name {
                Some(config.library_name.clone())
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    if !allows_caching {
        tracing::info!(
            "Peer {} does not allow library caching, skipping book sync",
            peer_url
        );
        // Still sync gamification stats if available
        sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;
        // Still update last_seen (and name if changed)
        if let Ok(Some(peer)) = crate::models::peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
        {
            let mut active_peer: peer::ActiveModel = peer.into();
            if let Some(ref new_name) = updated_name {
                active_peer.name = Set(new_name.clone());
                tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
            }
            active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active_peer.update(db).await;
        }
        return Ok(0); // Return 0 books cached
    }

    // Fetch remote books
    let url = format!("{}/api/books", peer_url);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to contact peer: {}", e))?;

    if !response.status().is_success() {
        return Err("Peer returned error".to_string());
    }

    // Parse response
    #[derive(Deserialize)]
    struct BooksResponse {
        books: Vec<crate::models::Book>,
    }

    let data: BooksResponse = response
        .json()
        .await
        .map_err(|_| "Invalid response format".to_string())?;

    // Clear old cache
    let _ = peer_book::Entity::delete_many()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .exec(db)
        .await;

    let count = data.books.len();

    // Insert new cache
    for book in data.books {
        let cache = peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set(book.id.unwrap_or(0)),
            title: Set(book.title),
            isbn: Set(book.isbn),
            author: Set(book.author),
            cover_url: Set(book.cover_url),
            summary: Set(book.summary),
            synced_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let _ = peer_book::Entity::insert(cache).exec(db).await;
    }

    // Sync gamification stats if both sides have the module enabled
    sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;

    // Update peer's last_seen (and name if changed)
    if let Ok(Some(peer)) = crate::models::peer::Entity::find_by_id(peer_id)
        .one(db)
        .await
    {
        let mut active_peer: peer::ActiveModel = peer.into();
        if let Some(ref new_name) = updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active_peer.update(db).await;
    }

    tracing::info!(
        "✅ Background sync completed: {} books cached for peer {}",
        count,
        peer_id
    );
    Ok(count)
}

/// Sync gamification stats from a peer.
/// `peer_shares_stats`:
///   - `Some(true)`:  peer confirmed it shares → fetch fresh stats
///   - `Some(false)`: peer confirmed it does NOT share → delete cached stats
///   - `None`:        peer was unreachable (config unknown) → preserve cache, skip sync
pub(crate) async fn sync_peer_gamification_stats(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
    client: &reqwest::Client,
    peer_shares_stats: Option<bool>,
) {
    use crate::models::installation_profile;

    // Check if network_gamification is enabled locally
    let local_enabled = match installation_profile::Entity::find_by_id(1).one(db).await {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"network_gamification".to_string())
        }
        _ => false,
    };

    if !local_enabled {
        return;
    }

    match peer_shares_stats {
        None => {
            // Peer unreachable — preserve cached data
            tracing::debug!(
                "Peer {} config unknown, preserving cached gamification stats",
                peer_url
            );
            return;
        }
        Some(false) => {
            // Peer explicitly does NOT share stats — clean up cache
            let _ = peer_gamification_stats::Entity::delete_many()
                .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
                .exec(db)
                .await;
            return;
        }
        Some(true) => {} // Peer shares — continue to fetch
    }

    // Fetch peer's public gamification stats
    let stats_url = format!("{}/api/gamification/public-stats", peer_url);
    let stats = match client.get(&stats_url).send().await {
        Ok(res) if res.status().is_success() => {
            match res
                .json::<crate::api::gamification::PublicGamificationStats>()
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to parse gamification stats from peer: {}", e);
                    return;
                }
            }
        }
        _ => {
            tracing::warn!("Failed to fetch gamification stats from peer {}", peer_url);
            return;
        }
    };

    // Upsert: delete old + insert new (same pattern as peer_books)
    let _ = peer_gamification_stats::Entity::delete_many()
        .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
        .exec(db)
        .await;

    let entry = peer_gamification_stats::ActiveModel {
        peer_id: Set(peer_id),
        library_name: Set(stats.library_name),
        collector_level: Set(stats.collector.level),
        collector_current: Set(stats.collector.current as i32),
        reader_level: Set(stats.reader.level),
        reader_current: Set(stats.reader.current as i32),
        lender_level: Set(stats.lender.level),
        lender_current: Set(stats.lender.current as i32),
        cataloguer_level: Set(stats.cataloguer.level),
        cataloguer_current: Set(stats.cataloguer.current as i32),
        synced_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    if let Err(e) = peer_gamification_stats::Entity::insert(entry)
        .exec(db)
        .await
    {
        tracing::warn!("Failed to save peer gamification stats: {}", e);
    } else {
        tracing::info!("Gamification stats synced for peer {}", peer_id);
    }
}

#[derive(Deserialize)]
pub struct IncomingConnectionRequest {
    name: String,
    url: String,
    /// Ed25519 public key (hex) from the requesting peer — for E2EE
    #[serde(default)]
    ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the requesting peer — for E2EE
    #[serde(default)]
    x25519_public_key: Option<String>,
    /// Peer's relay hub URL
    #[serde(default)]
    relay_url: Option<String>,
    /// Peer's relay mailbox UUID
    #[serde(default)]
    mailbox_id: Option<String>,
    /// Token to write to peer's relay mailbox
    #[serde(default)]
    relay_write_token: Option<String>,
}

/// Receive an incoming connection request from a remote peer.
/// Tries to forward to the local Hub first; falls back to local storage
/// if the Hub is unreachable or rejects the request (P2P/FFI mode).
pub async fn receive_connection_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingConnectionRequest>,
) -> impl IntoResponse {
    // Try forwarding to local Hub
    let hub_url = std::env::var("HUB_URL").unwrap_or_else(|_| "http://localhost:8081".to_string());
    let endpoint = format!("{}/api/peers/receive_connection", hub_url);

    let client = get_safe_client();
    let hub_result = client
        .post(&endpoint)
        .json(&serde_json::json!({
            "name": payload.name,
            "url": payload.url,
        }))
        .send()
        .await;

    // If Hub handled it successfully, we're done
    if let Ok(ref res) = hub_result
        && res.status().is_success()
    {
        return (
            StatusCode::OK,
            Json(json!({ "message": "Connection request received and forwarded to Hub" })),
        )
            .into_response();
    }

    // Hub unreachable or rejected — handle locally (P2P/FFI mode)
    let existing = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .one(&db)
        .await;

    // Load our own public keys to include in the response
    let (my_ed25519, my_x25519) = crate::api::setup::load_public_keys_from_db(&db).await;

    // Determine if peer sent E2EE keys
    let key_exchange_done =
        payload.ed25519_public_key.is_some() && payload.x25519_public_key.is_some();

    match existing {
        Ok(Some(existing_peer)) => {
            // Peer already exists — update keys and relay info if provided
            if key_exchange_done && !existing_peer.key_exchange_done {
                let mut active: peer::ActiveModel = existing_peer.into();
                active.public_key = Set(payload.ed25519_public_key);
                active.x25519_public_key = Set(payload.x25519_public_key);
                active.key_exchange_done = Set(true);
                if payload.relay_url.is_some() {
                    active.relay_url = Set(payload.relay_url);
                }
                if payload.mailbox_id.is_some() {
                    active.mailbox_id = Set(payload.mailbox_id);
                }
                if payload.relay_write_token.is_some() {
                    active.relay_write_token = Set(payload.relay_write_token);
                }
                active.updated_at = Set(Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
            }

            // Load our relay config to include in response
            let my_relay = crate::api::relay::get_my_relay_config(&db).await;

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer already exists locally",
                    "ed25519_public_key": my_ed25519,
                    "x25519_public_key": my_x25519,
                    "relay_url": my_relay.as_ref().map(|r| &r.relay_url),
                    "mailbox_id": my_relay.as_ref().map(|r| &r.mailbox_uuid),
                    "relay_write_token": my_relay.as_ref().map(|r| &r.write_token),
                })),
            )
                .into_response()
        }
        Ok(None) => {
            // Check if connection_validation module is enabled
            let connection_status = if is_connection_validation_enabled(&db).await {
                "pending"
            } else {
                "accepted"
            };

            let new_peer = peer::ActiveModel {
                name: Set(payload.name),
                url: Set(payload.url),
                public_key: Set(payload.ed25519_public_key),
                x25519_public_key: Set(payload.x25519_public_key),
                key_exchange_done: Set(key_exchange_done),
                relay_url: Set(payload.relay_url),
                mailbox_id: Set(payload.mailbox_id),
                relay_write_token: Set(payload.relay_write_token),
                auto_approve: Set(connection_status == "accepted"),
                connection_status: Set(connection_status.to_string()),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };

            // Load our relay config to include in response
            let my_relay = crate::api::relay::get_my_relay_config(&db).await;

            match new_peer.insert(&db).await {
                Ok(_) => (
                    StatusCode::OK,
                    Json(json!({
                        "message": "Connection request saved locally",
                        "connection_status": connection_status,
                        "ed25519_public_key": my_ed25519,
                        "x25519_public_key": my_x25519,
                        "relay_url": my_relay.as_ref().map(|r| &r.relay_url),
                        "mailbox_id": my_relay.as_ref().map(|r| &r.mailbox_uuid),
                        "relay_write_token": my_relay.as_ref().map(|r| &r.write_token),
                    })),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to save peer locally: {}", e) })),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Database error: {}", e) })),
        )
            .into_response(),
    }
}

pub async fn list_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // 1. Sync with Hub if HUB_URL is set
    if let Ok(hub_url) = std::env::var("HUB_URL") {
        let client = get_safe_client();
        let url = format!("{}/api/peers", hub_url);

        if let Ok(res) = client.get(&url).send().await
            && res.status().is_success()
        {
            #[derive(Deserialize)]
            struct HubPeer {
                name: String,
                url: String,
                #[serde(rename = "status")]
                _status: String,
            }
            #[derive(Deserialize)]
            struct HubResponse {
                data: Vec<HubPeer>,
            }

            if let Ok(hub_res) = res.json::<HubResponse>().await {
                for hub_peer in hub_res.data {
                    // Normalize URL
                    let docker_url = translate_url_for_docker(&hub_peer.url);

                    // Check if peer exists
                    let existing = peer::Entity::find()
                        .filter(peer::Column::Url.eq(&docker_url))
                        .one(&db)
                        .await;

                    match existing {
                        Ok(Some(p)) => {
                            // Update status if changed
                            let mut active: peer::ActiveModel = p.into();
                            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                            let _ = active.update(&db).await;
                        }
                        Ok(None) => {
                            // Insert new peer
                            let new_peer = peer::ActiveModel {
                                name: Set(hub_peer.name),
                                url: Set(docker_url),
                                created_at: Set(chrono::Utc::now().to_rfc3339()),
                                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                                ..Default::default()
                            };
                            let _ = peer::Entity::insert(new_peer).exec(&db).await;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let peers = peer::Entity::find().all(&db).await.unwrap_or(vec![]);

    // Convert to JSON with computed status field
    let peers_with_status: Vec<serde_json::Value> = peers
        .into_iter()
        .map(|p| {
            let status = if p.connection_status == "pending" {
                "pending"
            } else {
                "connected"
            };
            json!({
                "id": p.id,
                "name": p.name,
                "url": p.url,
                "public_key": p.public_key,
                "latitude": p.latitude,
                "longitude": p.longitude,
                "auto_approve": p.auto_approve,
                "connection_status": p.connection_status,
                "status": status,
                "relay_url": p.relay_url,
                "mailbox_id": p.mailbox_id,
                "relay_write_token": p.relay_write_token,
                "last_seen": p.last_seen,
                "created_at": p.created_at,
                "updated_at": p.updated_at,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "data": peers_with_status
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct UpdatePeerStatusRequest {
    status: String, // "active" (accept) or "rejected"
}

/// Update a peer's status (accept or reject a connection request)
pub async fn update_peer_status(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerStatusRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // If rejecting, delete the peer entirely
    if payload.status == "rejected" {
        match peer::Entity::delete_by_id(peer_id).exec(&db).await {
            Ok(_) => {
                tracing::info!("🗑️ Peer {} rejected and deleted", peer_id);
                return (
                    StatusCode::OK,
                    Json(json!({
                        "message": "Peer rejected and removed",
                        "peer_id": peer_id
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
                )
                    .into_response();
            }
        }
    }

    // Update auto_approve and connection_status for accept/active
    let auto_approve = payload.status == "active" || payload.status == "accepted";

    let mut active_model: peer::ActiveModel = peer.into();
    active_model.auto_approve = Set(auto_approve);
    if auto_approve {
        active_model.connection_status = Set("accepted".to_string());
    }
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active_model.update(&db).await {
        Ok(updated) => {
            tracing::info!(
                "✅ Peer {} accepted, auto_approve={}",
                peer_id,
                auto_approve
            );
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer accepted",
                    "peer": updated,
                    "auto_approve": auto_approve
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdatePeerUrlRequest {
    pub url: String,
}

/// Update a peer's URL (for mDNS IP changes)
/// Security: Only pending peers can have their URL updated
pub async fn update_peer_url(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerUrlRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // Security: Only update URL for pending peers
    if peer.auto_approve {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Cannot update URL for connected peers" })),
        )
            .into_response();
    }

    // Check if URL is already taken by another peer
    if let Ok(Some(existing_peer)) = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .filter(peer::Column::Id.ne(peer_id))
        .one(&db)
        .await
    {
        // If the existing peer currently holding this URL is pending (not auto_approve),
        // we can assume it's a stale entry (e.g. from a previous mDNS discovery on this IP)
        // and delete it to free up the URL.
        if !existing_peer.auto_approve {
            tracing::info!(
                "♻️ deleting stale peer {} to free up URL {}",
                existing_peer.id,
                payload.url
            );
            let _ = peer::Entity::delete_by_id(existing_peer.id).exec(&db).await;
        } else {
            // If it's an approved peer, we can't just delete it.
            // This is a genuine conflict (two trusted peers on same IP? or same peer different ID?)
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "URL already in use by another trusted peer" })),
            )
                .into_response();
        }
    }

    let mut active_model: peer::ActiveModel = peer.into();
    active_model.url = Set(payload.url.clone());
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active_model.update(&db).await {
        Ok(updated) => {
            tracing::info!("✅ Peer {} URL updated to: {}", peer_id, payload.url);
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer URL updated",
                    "peer": updated
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

pub async fn delete_peer(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    match peer::Entity::delete_by_id(peer_id).exec(&db).await {
        Ok(_) => {
            tracing::info!("🗑️ Peer {} deleted", peer_id);
            (StatusCode::OK, Json(json!({ "message": "Peer deleted" }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct PushRequest {
    operations: Vec<OperationDto>,
}

#[derive(Serialize, Deserialize)]
pub struct OperationDto {
    entity_type: String,
    entity_id: i32,
    operation: String,
    payload: Option<String>,
    created_at: String,
}

pub async fn push_operations(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<PushRequest>,
) -> impl IntoResponse {
    // Simplified: just log them for now, in real app we'd apply them
    for op in payload.operations {
        let log = operation_log::ActiveModel {
            entity_type: Set(op.entity_type),
            entity_id: Set(op.entity_id),
            operation: Set(op.operation),
            payload: Set(op.payload),
            created_at: Set(op.created_at),
            ..Default::default()
        };
        let _ = operation_log::Entity::insert(log).exec(&db).await;
    }
    (
        StatusCode::OK,
        Json(json!({ "message": "Operations received" })),
    )
        .into_response()
}

pub async fn pull_operations(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let ops = operation_log::Entity::find()
        .all(&db)
        .await
        .unwrap_or(vec![]);
    (StatusCode::OK, Json(ops)).into_response()
}

#[derive(Deserialize)]
pub struct SearchRequest {
    query: String,
}

pub async fn search_local(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<SearchRequest>,
) -> impl IntoResponse {
    use crate::models::book;

    // Simple LIKE search for now
    let books = book::Entity::find()
        .filter(book::Column::Title.contains(&payload.query))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let book_dtos: Vec<crate::models::Book> =
        books.into_iter().map(crate::models::Book::from).collect();
    (StatusCode::OK, Json(book_dtos)).into_response()
}

#[derive(Deserialize)]
pub struct ProxySearchRequest {
    peer_id: Option<i32>,
    peer_url: Option<String>,
    query: String,
}

pub async fn proxy_search(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<ProxySearchRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find peer by id or url
    let peer = if let Some(id) = payload.peer_id {
        peer::Entity::find_by_id(id).one(db).await.unwrap_or(None)
    } else if let Some(ref url) = payload.peer_url {
        peer::Entity::find()
            .filter(peer::Column::Url.eq(url.as_str()))
            .one(db)
            .await
            .unwrap_or(None)
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer_id or peer_url required" })),
        )
            .into_response();
    };

    if let Some(peer) = peer {
        // Validate Peer URL (just in case it was modified in DB)
        if let Err(e) = validate_url(&peer.url) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }

        // Try E2EE path first (search is request-response: returns encrypted results)
        match try_send_e2ee(
            &state,
            &peer,
            "search_request",
            json!({ "query": payload.query }),
        )
        .await
        {
            Ok(Some(Some(response_msg))) => {
                // Got encrypted search results
                let results: Vec<crate::models::Book> = serde_json::from_value(
                    response_msg
                        .payload
                        .get("results")
                        .cloned()
                        .unwrap_or(json!([])),
                )
                .unwrap_or_default();
                return (StatusCode::OK, Json(results)).into_response();
            }
            Ok(Some(None)) => {
                // E2EE sent but no response body (unexpected for search)
                return (StatusCode::OK, Json(Vec::<crate::models::Book>::new())).into_response();
            }
            Ok(None) => {} // Fallback to plaintext
            Err(e) => {
                tracing::warn!("E2EE proxy_search failed, falling back to plaintext: {}", e);
            }
        }

        // 2. Legacy plaintext fallback
        let client = get_safe_client();
        let res = if payload.query.is_empty() {
            // Empty query = fetch all books
            let url = format!("{}/api/books", peer.url);
            client.get(&url).send().await
        } else {
            let url = format!("{}/api/peers/search", peer.url);
            client
                .post(&url)
                .json(&json!({ "query": payload.query }))
                .send()
                .await
        };

        match res {
            Ok(response) => {
                if response.status().is_success() {
                    // /api/books returns {"books": [...], "total": N}
                    // /api/peers/search returns [...]
                    let body: serde_json::Value = response.json().await.unwrap_or(json!([]));
                    let books: Vec<crate::models::Book> = if let Some(arr) = body.get("books") {
                        serde_json::from_value(arr.clone()).unwrap_or_default()
                    } else {
                        serde_json::from_value(body).unwrap_or_default()
                    };
                    return (StatusCode::OK, Json(books)).into_response();
                }
            }
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Failed to contact peer" })),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "Peer not found" })),
    )
        .into_response()
}

pub async fn sync_peer(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    // Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // 2. Validate URL and fetch remote books
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();

    // Check peer config for gamification sharing
    let config_url = format!("{}/api/config", peer.url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);

    let url = format!("{}/api/books", peer.url);

    let res = client.get(&url).send().await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // Parse response: { "books": [...] }
                #[derive(Deserialize)]
                struct BooksResponse {
                    books: Vec<crate::models::Book>,
                }

                match response.json::<BooksResponse>().await {
                    Ok(data) => {
                        // 3. Clear old cache for this peer
                        let _ = peer_book::Entity::delete_many()
                            .filter(peer_book::Column::PeerId.eq(peer.id))
                            .exec(&db)
                            .await;

                        let count = data.books.len();

                        // 4. Insert new cache
                        for book in data.books {
                            let cache = peer_book::ActiveModel {
                                peer_id: Set(peer.id),
                                remote_book_id: Set(book.id.unwrap_or(0)),
                                title: Set(book.title),
                                isbn: Set(book.isbn),
                                author: Set(book.author),
                                cover_url: Set(book.cover_url),
                                summary: Set(book.summary),
                                synced_at: Set(chrono::Utc::now().to_rfc3339()),
                                ..Default::default()
                            };
                            let _ = peer_book::Entity::insert(cache).exec(&db).await;
                        }

                        // Sync gamification stats
                        sync_peer_gamification_stats(
                            &db,
                            peer.id,
                            &peer.url,
                            &client,
                            shares_gamification,
                        )
                        .await;

                        (
                            StatusCode::OK,
                            Json(json!({ "message": "Sync successful", "count": count })),
                        )
                            .into_response()
                    }
                    _ => (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "Invalid response format" })),
                    )
                        .into_response(),
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned error" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

/// Sync peer by URL (solves ID mismatch between Hub and Backend)
pub async fn sync_peer_by_url(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;
    let db = state.db().clone();

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // 1. Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            // Peer not found locally, try to fetch from Hub
            let mut found_peer = None;

            if let Ok(hub_url) = std::env::var("HUB_URL") {
                let client = get_safe_client();
                let url = format!("{}/api/peers", hub_url);

                if let Ok(res) = client.get(&url).send().await
                    && res.status().is_success()
                {
                    #[derive(Deserialize)]
                    struct HubPeer {
                        name: String,
                        url: String,
                        #[serde(rename = "status")]
                        _status: String,
                    }
                    #[derive(Deserialize)]
                    struct HubResponse {
                        data: Vec<HubPeer>,
                    }

                    if let Ok(hub_res) = res.json::<HubResponse>().await {
                        for hub_peer in hub_res.data {
                            let hub_docker_url = translate_url_for_docker(&hub_peer.url);

                            // Match by URL
                            if hub_docker_url == docker_url {
                                // Insert new peer
                                let new_peer = peer::ActiveModel {
                                    name: Set(hub_peer.name),
                                    url: Set(hub_docker_url.clone()),
                                    created_at: Set(chrono::Utc::now().to_rfc3339()),
                                    updated_at: Set(chrono::Utc::now().to_rfc3339()),
                                    ..Default::default()
                                };

                                if let Ok(res) = peer::Entity::insert(new_peer).exec(&db).await {
                                    // Fetch the inserted peer to return it
                                    found_peer = peer::Entity::find_by_id(res.last_insert_id)
                                        .one(&db)
                                        .await
                                        .unwrap_or(None);
                                }
                                break;
                            }
                        }
                    }
                }
            }

            match found_peer {
                Some(p) => p,
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(
                            json!({ "error": format!("Peer not found with URL: {}", docker_url) }),
                        ),
                    )
                        .into_response();
                }
            }
        }
    };

    // 2. Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // 3. Validate URL
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();

    // 4. Check peer's config for privacy consent flags
    let config_url = format!("{}/api/config", peer.url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };

    let allows_caching = peer_config
        .as_ref()
        .is_some_and(|c| c.allow_library_caching);
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);

    // Extract updated name from peer config (if changed)
    let updated_name = peer_config
        .as_ref()
        .filter(|c| c.library_name != peer.name)
        .map(|c| c.library_name.clone());

    if !allows_caching {
        // Peer doesn't allow caching - still sync gamification stats
        sync_peer_gamification_stats(&db, peer.id, &peer.url, &client, shares_gamification).await;

        let peer_id = peer.id;
        let mut active_peer: peer::ActiveModel = peer.into();
        if let Some(ref new_name) = updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active_peer.update(&db).await;

        return (
            StatusCode::OK,
            Json(json!({
                "message": "Peer does not allow library caching",
                "count": 0,
                "peer_id": peer_id,
                "caching_allowed": false
            })),
        )
            .into_response();
    }

    // 4. Fetch remote books — try E2EE first, then plaintext fallback
    let books: Vec<crate::models::Book> =
        match try_send_e2ee(&state, &peer, "book_sync_request", json!({})).await {
            Ok(Some(Some(response_msg))) => {
                // Got encrypted book list
                serde_json::from_value(
                    response_msg
                        .payload
                        .get("books")
                        .cloned()
                        .unwrap_or(json!([])),
                )
                .unwrap_or_default()
            }
            Ok(Some(None)) => {
                // E2EE sent but no response body (unexpected for sync)
                vec![]
            }
            Ok(None) | Err(_) => {
                // Fallback to plaintext
                let url = format!("{}/api/books", peer.url);
                match client.get(&url).send().await {
                    Ok(response) if response.status().is_success() => {
                        #[derive(Deserialize)]
                        struct BooksResponse {
                            books: Vec<crate::models::Book>,
                        }
                        response
                            .json::<BooksResponse>()
                            .await
                            .map(|d| d.books)
                            .unwrap_or_default()
                    }
                    Ok(_) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": "Peer returned error" })),
                        )
                            .into_response();
                    }
                    Err(_) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": "Failed to contact peer" })),
                        )
                            .into_response();
                    }
                }
            }
        };

    // 5. Clear old cache and insert new books
    let _ = peer_book::Entity::delete_many()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .exec(&db)
        .await;

    let count = books.len();

    for book in books {
        let cache = peer_book::ActiveModel {
            peer_id: Set(peer.id),
            remote_book_id: Set(book.id.unwrap_or(0)),
            title: Set(book.title),
            isbn: Set(book.isbn),
            author: Set(book.author),
            cover_url: Set(book.cover_url),
            summary: Set(book.summary),
            synced_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let _ = peer_book::Entity::insert(cache).exec(&db).await;
    }

    // 6. Sync gamification stats
    sync_peer_gamification_stats(&db, peer.id, &peer.url, &client, shares_gamification).await;

    // 7. Update peer's last_seen (and name if changed)
    let peer_id = peer.id;
    let mut active_peer: peer::ActiveModel = peer.into();
    if let Some(ref new_name) = updated_name {
        active_peer.name = Set(new_name.clone());
        tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
    }
    active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
    active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
    let _ = active_peer.update(&db).await;

    (
        StatusCode::OK,
        Json(json!({ "message": "Sync successful", "count": count, "peer_id": peer_id })),
    )
        .into_response()
}

// --- Federated Search Helper ---

pub async fn broadcast_search(
    db: &DatabaseConnection,
    params: &crate::api::search::SearchQuery,
) -> Vec<crate::models::Book> {
    let peers = peer::Entity::find().all(db).await.unwrap_or(vec![]);
    if peers.is_empty() {
        return vec![];
    }

    let client = get_safe_client();
    let query_str = params.title.clone().unwrap_or_default(); // Simple query for now

    let futures = peers.into_iter().map(|peer| {
        let client = client.clone();
        let q = query_str.clone();
        async move {
            if validate_url(&peer.url).is_err() {
                return vec![];
            }
            let url = format!("{}/api/peers/search", peer.url);
            match client
                .post(&url)
                .json(&json!({ "query": q }))
                .timeout(std::time::Duration::from_secs(2)) // 2s timeout
                .send()
                .await
            {
                Ok(res) => {
                    match res.json::<Vec<crate::models::Book>>().await {
                        Ok(mut books) => {
                            // Tag source and embed peer_id for request
                            for b in &mut books {
                                b.source = Some(format!("Peer: {}", peer.name));
                                // Hack: Embed peer_id in source_data so frontend can use it
                                b.source_data = Some(json!({ "peer_id": peer.id }).to_string());
                            }
                            books
                        }
                        _ => {
                            vec![]
                        }
                    }
                }
                Err(_) => vec![],
            }
        }
    });

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}

#[derive(Deserialize)]
pub struct BookRequest {
    book_isbn: String,
    book_title: String,
}

pub async fn request_book(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<BookRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    // 2. Save Outgoing Request
    let outgoing_id = uuid::Uuid::new_v4().to_string();
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(outgoing_id.clone()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        lender_request_id: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(db)
        .await
    {
        tracing::error!("❌ Failed to save outgoing status: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 3. Send request to peer
    if let Err(e) = validate_url(&peer.url) {
        tracing::error!("❌ Invalid peer URL for request: {} ({})", peer.url, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    // Try E2EE path first
    let my_config = match crate::models::library_config::Entity::find().one(db).await {
        Ok(Some(config)) => config,
        _ => {
            tracing::error!("❌ Library config not found when sending request");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response();
        }
    };

    let e2ee_payload = json!({
        "from_peer_url": crate::utils::net::get_public_url(8000),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(_)) => {
            // E2EE succeeded
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)" })),
            )
                .into_response();
        }
        Ok(None) => {
            // Peer doesn't support E2EE, fall through to plaintext
        }
        Err(e) => {
            // E2EE transport error — message MAY have been delivered.
            // Do NOT fall back to plaintext to avoid duplicate requests.
            tracing::warn!("E2EE send failed (no plaintext fallback): {}", e);
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (e2ee error, no fallback)" })),
            )
                .into_response();
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response()
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct BookRequestByUrl {
    peer_url: String,
    book_isbn: String,
    book_title: String,
}

pub async fn request_book_by_url(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<BookRequestByUrl>,
) -> impl IntoResponse {
    let db = state.db();

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(&payload.peer_url);

    // 1. Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found with URL: {}", docker_url) })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // 2. Save Outgoing Request
    let outgoing_id = uuid::Uuid::new_v4().to_string();
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(outgoing_id.clone()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        lender_request_id: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(db)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 3. Send request to peer
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    // Get my config to identify myself
    let my_config = match crate::models::library_config::Entity::find().one(db).await {
        Ok(Some(config)) => config,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response();
        }
    };

    let e2ee_payload = json!({
        "from_peer_url": crate::utils::net::get_public_url(8000),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    // Try E2EE path first
    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(_)) => {
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)" })),
            )
                .into_response();
        }
        Ok(None) => {
            // E2EE not available for this peer — fall back to plaintext.
        }
        Err(e) => {
            // E2EE transport error — message MAY have been delivered.
            // Do NOT fall back to plaintext to avoid duplicate requests.
            tracing::warn!("E2EE loan_request error (no plaintext fallback): {e}");
            return (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response();
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response()
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

pub async fn list_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let requests = crate::models::p2p_outgoing_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "status": req.status,
                "created_at": req.created_at,
                "peer_name": peer.map(|p| p.name).unwrap_or("Unknown".to_string())
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

#[derive(Deserialize)]
pub struct IncomingRequest {
    from_peer_url: String,
    from_peer_name: String,
    book_isbn: String,
    book_title: String,
    requester_request_id: Option<String>,
}

pub async fn receive_request(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<IncomingRequest>,
) -> impl IntoResponse {
    let db = state.db().clone();

    // 1. Find or Create Peer
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.from_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_peer_name),
                url: Set(payload.from_peer_url),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // 2. Check if auto-approve should be used
    let auto_approve =
        is_auto_approve_loans_enabled(&db).await && peer.connection_status == "accepted";

    // 3. Create Request Record — always as "pending" initially
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = crate::models::p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        requester_request_id: Set(payload.requester_request_id),
    };

    match crate::models::p2p_request::Entity::insert(request)
        .exec(&db)
        .await
    {
        Ok(_) => {
            // If auto-approve is enabled, immediately accept the request
            if auto_approve {
                tracing::info!(
                    "Auto-approving loan request {} for peer {}",
                    request_id,
                    peer.name
                );
                let action = RequestAction {
                    status: "accepted".to_string(),
                };
                return update_request_status(State(state), Path(request_id), Json(action))
                    .await
                    .into_response();
            }

            (
                StatusCode::CREATED,
                Json(json!({ "success": true, "status": "pending" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let requests = crate::models::p2p_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "status": req.status,
                "created_at": req.created_at,
                "peer_name": peer.map(|p| p.name).unwrap_or("Unknown".to_string())
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

#[derive(Deserialize)]
pub struct RequestAction {
    pub status: String,
}

pub async fn update_request_status(
    State(state): State<crate::infrastructure::AppState>,
    Path(id): Path<String>,
    Json(payload): Json<RequestAction>,
) -> impl IntoResponse {
    use crate::models::{book, contact, copy, loan, p2p_request};
    let db = state.db().clone();

    let req = match p2p_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(r)) => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response();
        }
    };

    let mut active: p2p_request::ActiveModel = req.clone().into();
    let new_status = payload.status.as_str();

    // State transition logic
    if new_status == "accepted" && req.status == "pending" {
        // 1. Find Peer to link/create Contact
        let peer = match peer::Entity::find_by_id(req.from_peer_id).one(&db).await {
            Ok(Some(p)) => p,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "Peer not found" })),
                )
                    .into_response();
            }
        };

        // 2. Find Book and Available Copy
        tracing::info!(
            "Looking for book with ISBN: '{}' for request {}",
            req.book_isbn,
            req.id
        );
        let book = match book::Entity::find()
            .filter(book::Column::Isbn.eq(&req.book_isbn))
            .one(&db)
            .await
        {
            Ok(Some(b)) => {
                tracing::info!("Found book: {} (id={})", b.title, b.id);
                b
            }
            Ok(None) => {
                tracing::warn!(
                    "Book not found for ISBN: '{}'. Checking by title: '{}'",
                    req.book_isbn,
                    req.book_title
                );
                // Fallback: Try to find by title if ISBN lookup fails
                match book::Entity::find()
                    .filter(book::Column::Title.eq(&req.book_title))
                    .one(&db)
                    .await
                {
                    Ok(Some(b)) => {
                        tracing::info!("Found book by title: {} (id={})", b.title, b.id);
                        b
                    }
                    _ => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("Book not found (ISBN: '{}', Title: '{}')", req.book_isbn, req.book_title) })),
                        )
                            .into_response()
                    }
                }
            }
            Err(e) => {
                tracing::error!("DB error looking up book: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("DB error: {}", e) })),
                )
                    .into_response();
            }
        };

        let copy = match copy::Entity::find()
            .filter(copy::Column::BookId.eq(book.id))
            .filter(copy::Column::Status.eq("available"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            _ => {
                // Self-healing: Check if ANY copy exists
                let any_copy = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if any_copy.is_none() {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No copy found" })),
                    )
                        .into_response();
                } else {
                    // Copies exist but none are available (truly borrowed)
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No available copies" })),
                    )
                        .into_response();
                }
            }
        };

        // 3. Find or Create Contact for Peer
        let contact = match contact::Entity::find()
            .filter(contact::Column::Name.eq(&peer.name))
            .filter(contact::Column::Type.eq("Library"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                // Create new contact
                let new_contact = contact::ActiveModel {
                    r#type: Set("Library".to_string()),
                    name: Set(peer.name.clone()),
                    library_owner_id: Set(1), // Default owner
                    is_active: Set(true),
                    created_at: Set(chrono::Utc::now().to_rfc3339()),
                    updated_at: Set(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                };
                match new_contact.insert(&db).await {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("Failed to create contact: {}", e) })),
                        )
                            .into_response();
                    }
                }
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "DB Error finding contact" })),
                )
                    .into_response();
            }
        };

        // 4. Create Loan
        let loan = loan::ActiveModel {
            copy_id: Set(copy.id),
            contact_id: Set(contact.id),
            library_id: Set(1), // Default library
            loan_date: Set(chrono::Utc::now().to_rfc3339()),
            due_date: Set((chrono::Utc::now() + chrono::Duration::days(14)).to_rfc3339()), // 2 weeks default
            status: Set("active".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        if let Err(e) = loan::Entity::insert(loan).exec(&db).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create loan: {}", e) })),
            )
                .into_response();
        }

        // Update Copy status
        let mut active_copy: copy::ActiveModel = copy.into();
        active_copy.status = Set("loaned".to_string());
        info!(
            "Updating copy {} status to 'loaned' for loan acceptance",
            active_copy.id.clone().unwrap()
        );
        if let Err(e) = active_copy.update(&db).await {
            error!("Failed to update copy status to 'lent': {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to update copy status: {}", e) })),
            )
                .into_response();
        }

        // 5. Notify borrower that loan was accepted
        let peer_url = peer.url.clone();
        let book_isbn = book.isbn.clone();
        let book_title = book.title.clone();
        let book_cover = book.cover_url.clone();
        let due_date = (chrono::Utc::now() + chrono::Duration::days(14))
            .format("%Y-%m-%d")
            .to_string();

        // Get library name for lender identification
        let lender_name = match crate::models::library::Entity::find_by_id(1).one(&db).await {
            Ok(Some(lib)) => lib.name,
            _ => "Unknown Library".to_string(),
        };

        let confirm_payload = serde_json::json!({
            "isbn": book_isbn,
            "title": book_title,
            "author": Option::<String>::None,
            "cover_url": book_cover,
            "lender_name": lender_name,
            "due_date": due_date,
            "request_id": req.id,
        });

        // Try E2EE path first
        match try_send_e2ee(&state, &peer, "loan_confirmation", confirm_payload.clone()).await {
            Ok(Some(_)) => {
                tracing::info!("E2EE: Loan confirmation sent to {} (encrypted)", peer.name);
            }
            Err(e) => {
                // E2EE transport error — message MAY have been delivered.
                // Do NOT fall back to plaintext to avoid duplicate borrowed copies.
                tracing::warn!("E2EE: Loan confirmation error (no plaintext fallback): {e}");
            }
            Ok(None) => {
                // E2EE not available for this peer — fall back to plaintext
                let peer_url_clone = peer_url.clone();
                tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    let confirm_result = client
                        .post(format!("{}/api/peers/loans/confirm", peer_url_clone))
                        .json(&confirm_payload)
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await;

                    match confirm_result {
                        Ok(resp) => {
                            tracing::info!(
                                "Loan confirmation sent to {}: {}",
                                peer_url_clone,
                                resp.status()
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to send loan confirmation to {}: {}",
                                peer_url_clone,
                                e
                            );
                        }
                    }
                });
            }
        }
    } else if new_status == "returned" && req.status == "accepted" {
        // Handle Return
        // Find the loan associated with this peer (contact) and book
        // This is tricky because we didn't link Loan to Request directly.
        // We have to infer: Find active loan for this book's copy where contact matches peer.

        // 1. Find Peer/Contact
        let peer = match peer::Entity::find_by_id(req.from_peer_id).one(&db).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Peer not found" })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response();
            }
        };

        let contact = contact::Entity::find()
            .filter(contact::Column::Name.eq(&peer.name))
            .filter(contact::Column::Type.eq("Library"))
            .one(&db)
            .await
            .unwrap_or(None);

        if let Some(contact) = contact {
            let book = book::Entity::find()
                .filter(book::Column::Isbn.eq(&req.book_isbn))
                .one(&db)
                .await
                .unwrap_or(None);

            if let Some(book) = book {
                // 3. Find Active Loan for any copy of this book for this contact
                let copies = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id))
                    .all(&db)
                    .await
                    .unwrap_or(vec![]);

                let copy_ids: Vec<i32> = copies.iter().map(|c| c.id).collect();

                let active_loan = loan::Entity::find()
                    .filter(loan::Column::ContactId.eq(contact.id))
                    .filter(loan::Column::Status.eq("active"))
                    .filter(loan::Column::CopyId.is_in(copy_ids))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if let Some(l) = active_loan {
                    let mut active_loan: loan::ActiveModel = l.clone().into();
                    active_loan.status = Set("returned".to_string());
                    active_loan.return_date = Set(Some(chrono::Utc::now().to_rfc3339()));
                    active_loan.updated_at = Set(chrono::Utc::now().to_rfc3339());
                    let _ = active_loan.update(&db).await;

                    // Update Copy
                    if let Some(copy) = copy::Entity::find_by_id(l.copy_id)
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                    {
                        let mut active_copy: copy::ActiveModel = copy.into();
                        active_copy.status = Set("available".to_string());
                        let _ = active_copy.update(&db).await;
                    }
                }
            }
        }
    }

    // Update Request Status
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    // Notify borrower of status change
    let peer_for_notify = peer::Entity::find_by_id(req.from_peer_id)
        .one(&db)
        .await
        .ok()
        .flatten();

    if let Some(peer) = peer_for_notify {
        // Use the borrower's original request ID so they can match
        // the status update to their outgoing request. Fall back to
        // our local ID for backward compat with old peers.
        let borrower_loan_id = req
            .requester_request_id
            .clone()
            .unwrap_or_else(|| req.id.clone());

        let status_payload = json!({
            "loan_id": borrower_loan_id,
            "status": new_status,
        });

        // Try E2EE first
        match try_send_e2ee(&state, &peer, "status_update", status_payload).await {
            Ok(Some(_)) => {
                tracing::info!("E2EE: Status update sent to {} (encrypted)", peer.name);
            }
            Err(e) => {
                // E2EE transport error — message MAY have been delivered.
                // Do NOT fall back to plaintext to avoid duplicate status updates.
                tracing::warn!("E2EE: Status update error (no plaintext fallback): {e}");
            }
            Ok(None) => {
                // E2EE not available for this peer — fall back to plaintext
                let peer_url = peer.url.clone();
                let request_id = borrower_loan_id;
                let status_to_send = new_status.to_string();

                tokio::spawn(async move {
                    let client = get_safe_client();
                    let notify_url =
                        format!("{}/api/peers/requests/status/{}", peer_url, request_id);

                    tracing::info!(
                        "Notifying borrower {} of status change: {} -> {}",
                        peer_url,
                        request_id,
                        status_to_send
                    );

                    match client
                        .put(&notify_url)
                        .json(&serde_json::json!({ "status": status_to_send }))
                        .send()
                        .await
                    {
                        Ok(res) => {
                            tracing::info!("Borrower notified: {}", res.status());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to notify borrower: {}", e);
                        }
                    }
                });
            }
        }
    }

    match active.update(&db).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_peer_books(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Check if peer is approved
    if let Ok(Some(peer)) = peer::Entity::find_by_id(peer_id).one(&db).await
        && !is_peer_approved(&db, &peer).await
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// List peer books by URL (solves ID mismatch)
pub async fn list_peer_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found with URL: {}", docker_url) })),
            )
                .into_response();
        }
    };

    // Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // Get books for this peer
    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// Get cached peer books with staleness metadata (no network call to peer)
/// Returns books from local cache along with last_synced timestamp for UI staleness indicator
pub async fn get_cached_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Peer not found - return empty result with null metadata
            return (
                StatusCode::OK,
                Json(json!({
                    "books": [],
                    "peer_name": null,
                    "peer_id": null,
                    "last_synced": null,
                    "last_seen": null,
                    "cached": true
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // Get cached books for this peer
    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Get latest synced_at from cached books (all books have same sync time)
    let last_synced = books.first().map(|b| b.synced_at.clone());

    (
        StatusCode::OK,
        Json(json!({
            "books": books,
            "peer_name": peer.name,
            "peer_id": peer.id,
            "last_synced": last_synced,
            "last_seen": peer.last_seen,
            "cached": true
        })),
    )
        .into_response()
}

/// Cleanup peer_books entries older than 30 days (TTL for privacy)
/// Call this on app startup to auto-purge stale caches
pub async fn cleanup_stale_peer_books(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::peer_book;
    use sea_orm::QueryFilter;

    // Calculate cutoff date (30 days ago)
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let cutoff_str = cutoff.to_rfc3339();

    // Delete stale peer_books entries
    let books_deleted = peer_book::Entity::delete_many()
        .filter(peer_book::Column::SyncedAt.lt(&cutoff_str))
        .exec(&db)
        .await
        .map(|r| r.rows_affected)
        .unwrap_or(0);

    // Also clean up stale peer_gamification_stats
    let stats_deleted = peer_gamification_stats::Entity::delete_many()
        .filter(peer_gamification_stats::Column::SyncedAt.lt(&cutoff_str))
        .exec(&db)
        .await
        .map(|r| r.rows_affected)
        .unwrap_or(0);

    if books_deleted > 0 || stats_deleted > 0 {
        tracing::info!(
            "TTL cleanup: deleted {} stale peer_books + {} stale peer_gamification_stats (older than 30 days)",
            books_deleted,
            stats_deleted
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "deleted": books_deleted,
            "stats_deleted": stats_deleted,
            "cutoff": cutoff_str
        })),
    )
        .into_response()
}

pub async fn delete_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    match p2p_request::Entity::delete_by_id(id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Request not found" })),
                )
                    .into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_outgoing_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    // 1. First, retrieve the request to get the peer info
    let request = match p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 2. Get the peer URL to notify them
    let peer = match peer::Entity::find_by_id(request.to_peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::warn!(
                "Peer {} not found for outgoing request {}",
                request.to_peer_id,
                id
            );
            // Peer not found, just delete locally
            let _ = p2p_outgoing_request::Entity::delete_by_id(&id)
                .exec(&db)
                .await;
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 3. Notify the peer about the cancellation (best effort)
    let client = get_safe_client();
    let cancel_url = format!("{}/api/peers/requests/cancel/{}", peer.url, id);

    tracing::info!(
        "📡 Notifying peer {} of request cancellation: {}",
        peer.name,
        cancel_url
    );

    match client.delete(&cancel_url).send().await {
        Ok(res) => {
            if res.status().is_success() {
                tracing::info!("✅ Peer notified successfully");
            } else {
                tracing::warn!("⚠️ Peer notification returned: {}", res.status());
            }
        }
        Err(e) => {
            tracing::warn!("⚠️ Failed to notify peer (may be offline): {}", e);
            // Continue with local deletion anyway
        }
    }

    // 4. Delete locally
    match p2p_outgoing_request::Entity::delete_by_id(&id)
        .exec(&db)
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Receive cancellation notification from a peer who cancelled their outgoing request
pub async fn cancel_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    tracing::info!("📨 Received cancellation notification for request: {}", id);

    // Delete the incoming request that matches this ID
    match p2p_request::Entity::delete_by_id(&id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                tracing::warn!("Cancellation target not found: {}", id);
                // Return OK anyway - idempotent behavior
                StatusCode::OK.into_response()
            } else {
                tracing::info!("✅ Request {} cancelled successfully", id);
                StatusCode::OK.into_response()
            }
        }
        Err(e) => {
            tracing::error!("❌ Failed to cancel request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Receive status update notification from lender (updates local outgoing request)
pub async fn update_outgoing_status(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::{book, copy, p2p_outgoing_request};

    let new_status = match payload.get("status").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing status field" })),
            )
                .into_response();
        }
    };

    tracing::info!(
        "📨 Received status update for outgoing request {}: {}",
        id,
        new_status
    );

    // Find the outgoing request
    let request = match p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            tracing::warn!("Outgoing request not found: {}", id);
            // Return OK anyway - idempotent
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Clone book_isbn before converting to ActiveModel (need it for cleanup)
    let book_isbn = request.book_isbn.clone();

    // Update the status
    let mut active: p2p_outgoing_request::ActiveModel = request.into();
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(_) => {
            tracing::info!("✅ Outgoing request {} updated to {}", id, new_status);

            // If the loan is returned, clean up the borrowed copy
            if new_status == "returned" {
                tracing::info!("🧹 Cleaning up borrowed copy for book ISBN: {}", book_isbn);

                // 1. Find the book by ISBN
                if let Ok(Some(book)) = book::Entity::find()
                    .filter(book::Column::Isbn.eq(&book_isbn))
                    .one(&db)
                    .await
                {
                    // 2. Find and delete the borrowed copy
                    if let Ok(Some(borrowed_copy)) = copy::Entity::find()
                        .filter(copy::Column::BookId.eq(book.id))
                        .filter(copy::Column::Status.eq("borrowed"))
                        .one(&db)
                        .await
                    {
                        match copy::Entity::delete_by_id(borrowed_copy.id).exec(&db).await {
                            Err(e) => {
                                tracing::warn!("⚠️ Failed to delete borrowed copy: {}", e);
                            }
                            _ => {
                                tracing::info!(
                                    "✅ Deleted borrowed copy {} for book {}",
                                    borrowed_copy.id,
                                    book.id
                                );
                            }
                        }
                    }

                    // 3. Check if book should be deleted
                    // Conditions: owned=false, reading_status != wishlist, no copies left
                    let should_delete_book = !book.owned
                        && book.reading_status != "READING_STATUS_WISHLIST"
                        && copy::Entity::find()
                            .filter(copy::Column::BookId.eq(book.id))
                            .count(&db)
                            .await
                            .unwrap_or(1)
                            == 0;

                    if should_delete_book {
                        tracing::info!(
                            "🗑️ Book {} (ISBN: {}) has no more copies, not owned, not in wishlist - deleting",
                            book.id,
                            book_isbn
                        );
                        match book::Entity::delete_by_id(book.id).exec(&db).await {
                            Err(e) => {
                                tracing::warn!("⚠️ Failed to delete book: {}", e);
                            }
                            _ => {
                                tracing::info!("✅ Deleted book {} after loan return", book.id);
                            }
                        }
                    }
                }
            }

            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::error!("❌ Failed to update outgoing request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// ============ BORROWER-INITIATED RETURN ============

#[derive(Deserialize)]
pub struct ReturnBorrowedBookPayload {
    pub copy_id: i32,
}

/// Borrower initiates a return: notifies the lender and cleans up local data.
pub async fn return_borrowed_book(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<ReturnBorrowedBookPayload>,
) -> impl IntoResponse {
    use crate::models::{book, copy, p2p_outgoing_request, peer};
    let db = state.db().clone();

    tracing::info!(
        "📚 Borrower initiating return for copy_id: {}",
        payload.copy_id
    );

    // 1. Look up the copy to get book_id, then the book to get ISBN
    let the_copy = match copy::Entity::find_by_id(payload.copy_id).one(&db).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Copy not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let book_isbn = match book::Entity::find_by_id(the_copy.book_id).one(&db).await {
        Ok(Some(b)) => b.isbn.unwrap_or_default(),
        _ => String::new(),
    };

    // 2. Find the outgoing request for this book with status "accepted"
    let outgoing = if !book_isbn.is_empty() {
        p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(&book_isbn))
            .filter(p2p_outgoing_request::Column::Status.eq("accepted"))
            .one(&db)
            .await
    } else {
        Ok(None)
    };

    let outgoing_req = match outgoing {
        Ok(Some(req)) => req,
        Ok(None) => {
            tracing::warn!(
                "No accepted outgoing request found for ISBN: '{}'. Falling back to local cleanup.",
                book_isbn
            );
            // Fallback: just delete the local copy
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Copy deleted (no outgoing request found)" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("DB error finding outgoing request: {}", e);
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Copy deleted (db error on request lookup)" })),
            )
                .into_response();
        }
    };

    // 2. Find the peer (lender)
    let peer = match peer::Entity::find_by_id(outgoing_req.to_peer_id)
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::warn!("Peer not found for outgoing request");
            // Still clean up locally
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            let mut active: p2p_outgoing_request::ActiveModel = outgoing_req.into();
            active.status = Set("returned".to_string());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(&db).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Returned (peer not found)" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 3. Notify the lender to mark the loan as returned
    let lender_request_id = outgoing_req.lender_request_id.clone();
    if let Some(ref lender_req_id) = lender_request_id {
        let return_payload = json!({
            "loan_id": lender_req_id,
            "status": "returned",
        });

        // Try E2EE first
        match try_send_e2ee(&state, &peer, "status_update", return_payload).await {
            Ok(Some(_)) => {
                tracing::info!(
                    "E2EE: Return notification sent to {} (encrypted)",
                    peer.name
                );
            }
            Err(e) => {
                tracing::warn!("E2EE: Return notification error: {e}");
            }
            Ok(None) => {
                // Plaintext fallback
                let peer_url = peer.url.clone();
                let req_id = lender_req_id.clone();
                tokio::spawn(async move {
                    let client = get_safe_client();
                    let url = format!("{}/api/peers/requests/{}", peer_url, req_id);
                    match client
                        .put(&url)
                        .json(&serde_json::json!({ "status": "returned" }))
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await
                    {
                        Ok(res) => {
                            tracing::info!("Return notification sent to lender: {}", res.status());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to send return notification to lender: {}", e);
                        }
                    }
                });
            }
        }
    } else {
        tracing::warn!(
            "No lender_request_id on outgoing request — cannot notify lender. \
             Lender will need to mark the return manually."
        );
    }

    // 4. Update outgoing request status to "returned"
    let mut active: p2p_outgoing_request::ActiveModel = outgoing_req.into();
    active.status = Set("returned".to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    if let Err(e) = active.update(&db).await {
        tracing::warn!("Failed to update outgoing request status: {}", e);
    }

    // 5. Delete the borrowed copy
    if let Err(e) = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await {
        tracing::warn!("Failed to delete borrowed copy: {}", e);
    }

    // 6. Clean up book if no longer needed
    if let Ok(Some(bk)) = book::Entity::find_by_id(the_copy.book_id).one(&db).await {
        let should_delete = !bk.owned
            && bk.reading_status != "READING_STATUS_WISHLIST"
            && copy::Entity::find()
                .filter(copy::Column::BookId.eq(bk.id))
                .count(&db)
                .await
                .unwrap_or(1)
                == 0;

        if should_delete {
            let _ = book::Entity::delete_by_id(bk.id).exec(&db).await;
            tracing::info!("Deleted book {} after loan return", bk.id);
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "message": "Book returned successfully" })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct IncomingLoanRequest {
    pub from_name: String,
    pub from_url: String,
    pub library_uuid: Option<String>, // For P2P deduplication
    pub book_isbn: String,
    pub book_title: String,
}

pub async fn receive_loan_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingLoanRequest>,
) -> impl IntoResponse {
    use crate::models::p2p_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find or Create Peer (deduplicate by library_uuid if provided)
    let existing_peer = if let Some(ref uuid) = payload.library_uuid {
        // Try to find by UUID first (more stable)
        peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await
    } else {
        // Fallback to URL matching
        peer::Entity::find()
            .filter(peer::Column::Url.eq(&payload.from_url))
            .one(&db)
            .await
    };

    let peer = match existing_peer {
        Ok(Some(mut p)) => {
            // Update URL if changed (IP might have changed)
            if p.url != payload.from_url {
                tracing::info!(
                    "📝 Updating peer {} URL: {} -> {}",
                    p.name,
                    p.url,
                    payload.from_url
                );

                // Check for conflict: Does another peer already use this new URL?
                let conflict_peer = peer::Entity::find()
                    .filter(peer::Column::Url.eq(&payload.from_url))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if let Some(conflict) = conflict_peer {
                    // If conflict is NOT the same peer (ids differ), we have a problem.
                    // Since URLs must be unique and we trust the new incoming request (it's active right now),
                    // we assume the old entry holding this IP is stale.
                    if conflict.id != p.id {
                        tracing::warn!(
                            "⚠️ Found stale peer {} holding URL {}. Deleting it.",
                            conflict.name,
                            payload.from_url
                        );
                        let _ = peer::Entity::delete_by_id(conflict.id).exec(&db).await;
                    }
                }

                let mut active: peer::ActiveModel = p.clone().into();
                active.url = Set(payload.from_url.clone());
                active.updated_at = Set(Utc::now().to_rfc3339());
                if let Ok(updated) = active.update(&db).await {
                    p = updated;
                }
            }
            p
        }
        Ok(None) => {
            // Creating new peer. Check if URL is already taken by a stale peer (since UUID didn't match)
            let conflict_peer = peer::Entity::find()
                .filter(peer::Column::Url.eq(&payload.from_url))
                .one(&db)
                .await
                .unwrap_or(None);

            if let Some(conflict) = conflict_peer {
                tracing::warn!(
                    "⚠️ New peer claims URL {} held by old peer {}. Deleting old peer.",
                    payload.from_url,
                    conflict.name
                );
                let _ = peer::Entity::delete_by_id(conflict.id).exec(&db).await;
            }

            let conn_status = if is_connection_validation_enabled(&db).await {
                "pending"
            } else {
                "accepted"
            };
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_name),
                url: Set(payload.from_url),
                library_uuid: Set(payload.library_uuid),
                auto_approve: Set(conn_status == "accepted"),
                connection_status: Set(conn_status.to_string()),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Create Incoming Request
    let request_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Loan request received", "request_id": request_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save request: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct OutgoingLoanRequestDto {
    pub to_peer_url: String,
    pub book_isbn: String,
    pub book_title: String,
    pub request_id: Option<String>, // ID from remote peer for sync
}

pub async fn create_outgoing_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<OutgoingLoanRequestDto>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find Peer by URL, or auto-create if not found
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.to_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Auto-create peer from URL (will be updated on next mDNS discovery)
            tracing::info!(
                "📝 Auto-creating peer for outgoing request: {}",
                payload.to_peer_url
            );
            let new_peer = peer::ActiveModel {
                name: Set("Réseau local".to_string()), // Placeholder name
                url: Set(payload.to_peer_url.clone()),
                auto_approve: Set(false),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Create Outgoing Request Log
    // Use request_id from remote peer if provided (for sync), otherwise generate new
    let request_id = payload
        .request_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_outgoing_request::ActiveModel {
        id: Set(request_id),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Outgoing request logged" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save outgoing request: {}", e) })),
        )
            .into_response(),
    }
}

// ============ P2P LOAN CONFIRMATION ============

#[derive(Debug, Deserialize)]
pub struct LoanConfirmation {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub lender_name: String,
    pub due_date: String,
    pub request_id: Option<String>,
}

/// Receive loan confirmation from lender
/// Creates the book (if not exists) and a borrowed copy in the borrower's library
pub async fn receive_loan_confirmation(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoanConfirmation>,
) -> impl IntoResponse {
    use crate::models::{book, copy, p2p_outgoing_request};
    use chrono::Utc;

    tracing::info!(
        "📚 Received loan confirmation: '{}' from {}",
        payload.title,
        payload.lender_name
    );

    // 1. Find or create book
    let existing_book = if let Some(ref isbn) = payload.isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn))
            .one(&db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(&payload.title))
            .one(&db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => {
            tracing::info!("Book already exists: id={}", b.id);
            b.id
        }
        None => {
            // Create new book
            let now = Utc::now().to_rfc3339();
            // Note: author is a relation, not a direct field on books table
            // Store author info in summary for now
            let summary_text = payload.author.clone().map(|a| format!("Auteur: {}", a));
            let new_book = book::ActiveModel {
                title: Set(payload.title.clone()),
                isbn: Set(payload.isbn.clone()),
                summary: Set(summary_text),
                cover_url: Set(payload.cover_url.clone()),
                owned: Set(false), // It's a borrowed book, not owned
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };

            match new_book.insert(&db).await {
                Ok(b) => {
                    tracing::info!("Created new book: id={}", b.id);
                    b.id
                }
                Err(e) => {
                    tracing::error!("Failed to create book: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create book: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
    };

    // 2. Create borrowed copy
    let now = Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(1), // Default library
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {} jusqu'au {}",
            payload.lender_name, payload.due_date
        ))),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(&db).await {
        Ok(c) => {
            tracing::info!(
                "✅ Created borrowed copy: id={} for book_id={}",
                c.id,
                book_id
            );

            // Store lender_request_id on the matching outgoing request
            if let Some(ref lender_req_id) = payload.request_id {
                let isbn_filter = payload.isbn.clone().unwrap_or_default();
                if let Ok(Some(outgoing)) = p2p_outgoing_request::Entity::find()
                    .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                    .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                    .one(&db)
                    .await
                {
                    let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
                    active.lender_request_id = Set(Some(lender_req_id.clone()));
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    if let Err(e) = active.update(&db).await {
                        tracing::warn!(
                            "Failed to store lender_request_id on outgoing request: {}",
                            e
                        );
                    } else {
                        tracing::info!(
                            "✅ Stored lender_request_id={} on outgoing request",
                            lender_req_id
                        );
                    }
                }
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Loan confirmed",
                    "book_id": book_id,
                    "copy_id": c.id
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to create borrowed copy: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create copy: {}", e) })),
            )
                .into_response()
        }
    }
}
