//! Peer connection and disconnection lifecycle.

use super::*;
use crate::models::{peer, peer_book};
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

#[derive(Deserialize)]
pub struct ConnectRequest {
    name: String,
    url: String,
    public_key: Option<String>,
    /// Stable library UUID for P2P peer deduplication
    #[serde(default)]
    library_uuid: Option<String>,
    /// Ed25519 public key (hex) from the remote peer - for E2EE
    #[serde(default)]
    ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the remote peer - for E2EE
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
    // Relay-only peers have an empty URL — skip URL validation and remote
    // config fetch in that case. All data comes from the request payload.
    let is_relay_only = payload.url.is_empty();

    // 1. Validate URL (only for LAN peers with a real HTTP URL)
    if !is_relay_only && let Err(e) = validate_url(&payload.url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Fetch remote config to get location and verify connectivity
    struct RemoteConfigData {
        latitude: Option<f64>,
        longitude: Option<f64>,
        remote_name: Option<String>,
        library_uuid: Option<String>,
        ed25519_public_key: Option<String>,
        x25519_public_key: Option<String>,
        relay_url: Option<String>,
        mailbox_id: Option<String>,
        relay_write_token: Option<String>,
        avatar_config: Option<String>,
    }

    let (remote_data, remote_reachable) = if is_relay_only {
        // Relay-only: no remote config to fetch, all data from payload
        (
            RemoteConfigData {
                latitude: None,
                longitude: None,
                remote_name: None,
                library_uuid: None,
                ed25519_public_key: None,
                x25519_public_key: None,
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
                avatar_config: None,
            },
            false,
        )
    } else {
        let client = get_safe_client();
        let config_url = format!("{}/api/config", payload.url.trim_end_matches('/'));
        match client.get(&config_url).send().await {
            Ok(res) => {
                if res.status().is_success() {
                    match res.json::<crate::api::setup::ConfigResponse>().await {
                        Ok(config) => {
                            let (lat, long) = if config.share_location {
                                (config.latitude, config.longitude)
                            } else {
                                (None, None)
                            };
                            let avatar = config
                                .avatar_config
                                .map(|v| serde_json::to_string(&v).unwrap_or_default());
                            (
                                RemoteConfigData {
                                    latitude: lat,
                                    longitude: long,
                                    remote_name: Some(config.library_name),
                                    library_uuid: config.library_uuid,
                                    ed25519_public_key: config.ed25519_public_key,
                                    x25519_public_key: config.x25519_public_key,
                                    relay_url: config.relay_url,
                                    mailbox_id: config.mailbox_id,
                                    relay_write_token: config.relay_write_token,
                                    avatar_config: avatar,
                                },
                                true,
                            )
                        }
                        _ => (
                            RemoteConfigData {
                                latitude: None,
                                longitude: None,
                                remote_name: None,
                                library_uuid: None,
                                ed25519_public_key: None,
                                x25519_public_key: None,
                                relay_url: None,
                                mailbox_id: None,
                                relay_write_token: None,
                                avatar_config: None,
                            },
                            false,
                        ),
                    }
                } else {
                    (
                        RemoteConfigData {
                            latitude: None,
                            longitude: None,
                            remote_name: None,
                            library_uuid: None,
                            ed25519_public_key: None,
                            x25519_public_key: None,
                            relay_url: None,
                            mailbox_id: None,
                            relay_write_token: None,
                            avatar_config: None,
                        },
                        false,
                    )
                }
            }
            Err(_) => (
                RemoteConfigData {
                    latitude: None,
                    longitude: None,
                    remote_name: None,
                    library_uuid: None,
                    ed25519_public_key: None,
                    x25519_public_key: None,
                    relay_url: None,
                    mailbox_id: None,
                    relay_write_token: None,
                    avatar_config: None,
                },
                false,
            ),
        }
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

    // Library UUID: prefer payload (QR/invite), fall back to remote config
    let peer_library_uuid = payload.library_uuid.or(remote_data.library_uuid);

    // Translate localhost URLs to Docker service names for inter-container communication
    // For relay-only peers (empty URL), use a unique placeholder to satisfy
    // the NOT NULL UNIQUE constraint on peers.url (same pattern as relay_poller).
    let docker_url = if is_relay_only {
        let unique_part = peer_library_uuid
            .as_deref()
            .or(ed25519_key.as_deref())
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        format!("relay://{unique_part}")
    } else {
        translate_url_for_docker(&payload.url)
    };
    let peer_url_for_sync = docker_url.clone(); // Clone before moving into ActiveModel

    // Upsert: find existing peer by library_uuid first (most reliable),
    // then by URL (handles port changes from hot restarts).
    // For relay-only peers (empty URL), UUID is the only reliable key.
    let mut existing = if let Some(ref uuid) = peer_library_uuid {
        peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await
    } else {
        Ok(None)
    };

    if matches!(&existing, Ok(None)) && !docker_url.is_empty() {
        existing = peer::Entity::find()
            .filter(peer::Column::Url.eq(&docker_url))
            .one(&db)
            .await;
    }

    // Relay info: prefer payload, fall back to remote config
    let relay_url = payload.relay_url.or(remote_data.relay_url);
    let mailbox_id = payload.mailbox_id.or(remote_data.mailbox_id);
    let relay_write_token = payload.relay_write_token.or(remote_data.relay_write_token);

    // Clone for relay handshake (values will be moved into ActiveModel below)
    let relay_url_for_handshake = relay_url.clone();
    let mailbox_id_for_handshake = mailbox_id.clone();
    let relay_write_token_for_handshake = relay_write_token.clone();

    let peer_id = match existing {
        Ok(Some(existing_peer)) => {
            // Update existing peer with new keys and info
            let peer_id = existing_peer.id;
            let old_library_uuid = existing_peer.library_uuid.clone();
            let mut active: peer::ActiveModel = existing_peer.into();
            active.name = Set(name);
            active.url = Set(docker_url.clone()); // Update URL (port may have changed)
            active.library_uuid = Set(peer_library_uuid.clone());
            active.public_key = Set(ed25519_key.clone());
            active.x25519_public_key = Set(x25519_key);
            active.key_exchange_done = Set(key_exchange_done);
            active.latitude = Set(remote_data.latitude);
            active.longitude = Set(remote_data.longitude);
            active.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            active.auto_approve = Set(true);
            active.connection_status = Set("accepted".to_string());
            // Store avatar config if provided
            if remote_data.avatar_config.is_some() {
                active.avatar_config = Set(remote_data.avatar_config.clone());
            }
            // Store relay info if provided
            if relay_url.is_some() {
                active.relay_url = Set(relay_url);
            }
            if mailbox_id.is_some() {
                active.mailbox_id = Set(mailbox_id);
            }
            if relay_write_token.is_some() {
                active.relay_write_token = Set(relay_write_token);
                // ADR-032: fresh invitation clears any stale-token gate.
                active.relay_write_token_invalid_at = Set(None);
            }
            match active.update(&db).await {
                Ok(_) => {
                    // If library_uuid changed (peer was reset/reinstalled),
                    // clear cached books - the old library no longer exists.
                    let uuid_changed = match (&old_library_uuid, &peer_library_uuid) {
                        (Some(old), Some(new)) => old != new,
                        (None, Some(_)) => false, // first time getting uuid, keep cache
                        _ => false,
                    };
                    if uuid_changed {
                        // upsert_peer_books_cache (called by background sync)
                        // handles the transition atomically: insert new, update
                        // existing, delete absent. No premature cache wipe.
                        tracing::info!(
                            "Peer {} library_uuid changed, will refresh via upsert",
                            peer_id
                        );
                    }
                    peer_id
                }
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
                library_uuid: Set(peer_library_uuid),
                public_key: Set(ed25519_key.clone()),
                x25519_public_key: Set(x25519_key),
                key_exchange_done: Set(key_exchange_done),
                latitude: Set(remote_data.latitude),
                longitude: Set(remote_data.longitude),
                relay_url: Set(relay_url),
                mailbox_id: Set(mailbox_id),
                relay_write_token: Set(relay_write_token),
                avatar_config: Set(remote_data.avatar_config),
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

    // If the remote peer was unreachable (no WiFi) but we have their relay
    // credentials, tell Flutter to deposit the connection_request via native
    // HTTP (Dio). reqwest+rustls fails on iOS FFI, so the deposit is handled
    // by the Flutter caller using the native HTTP stack.
    if !remote_reachable
        && relay_url_for_handshake.is_some()
        && mailbox_id_for_handshake.is_some()
        && relay_write_token_for_handshake.is_some()
    {
        tracing::info!("Relay: Peer unreachable, relay_deposit_needed=true (Flutter will deposit)");
        return (
            StatusCode::CREATED,
            Json(json!({
                "id": peer_id,
                "relay_deposit_needed": true
            })),
        )
            .into_response();
    }

    (StatusCode::CREATED, Json(json!({ "id": peer_id }))).into_response()
}

#[derive(Deserialize)]
pub struct IncomingConnectionRequest {
    name: String,
    url: String,
    /// Stable library UUID for P2P peer deduplication
    #[serde(default)]
    library_uuid: Option<String>,
    /// Ed25519 public key (hex) from the requesting peer - for E2EE
    #[serde(default)]
    ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the requesting peer - for E2EE
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
/// Always creates/updates the peer in local SQLite and returns our E2EE keys.
/// Also forwards to the Hub (fire-and-forget) for the central directory.
pub async fn receive_connection_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingConnectionRequest>,
) -> impl IntoResponse {
    tracing::info!(
        "Peer: Received connection_request from '{}' (url='{}', e2ee={}, relay={}, library_uuid={:?})",
        payload.name,
        payload.url,
        payload.ed25519_public_key.is_some() && payload.x25519_public_key.is_some(),
        payload.relay_url.is_some(),
        payload.library_uuid
    );

    // Try forwarding to Hub (fire-and-forget for central directory).
    // Always continue to local handling regardless of hub result,
    // so the peer is created in our local SQLite and we return our E2EE keys.
    if let Ok(hub_url) = std::env::var("HUB_URL") {
        let endpoint = format!("{}/api/peers/receive_connection", hub_url);
        let client = get_safe_client();
        let _ = client
            .post(&endpoint)
            .json(&serde_json::json!({
                "name": payload.name,
                "url": payload.url,
            }))
            .send()
            .await;
    }

    // Always handle locally: create/update peer in SQLite + return our E2EE keys
    // Find by URL first, then by library_uuid (handles port changes)
    let mut existing = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .one(&db)
        .await;

    if matches!(&existing, Ok(None))
        && let Some(ref uuid) = payload.library_uuid
    {
        existing = peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await;
    }

    // Load our own public keys to include in the response
    let (my_ed25519, my_x25519) = crate::api::setup::load_public_keys_from_db(&db).await;

    // Determine if peer sent E2EE keys
    let key_exchange_done =
        payload.ed25519_public_key.is_some() && payload.x25519_public_key.is_some();

    match existing {
        Ok(Some(existing_peer)) => {
            // Peer already exists - update keys, relay info, and library_uuid if provided
            let old_uuid = existing_peer.library_uuid.clone();
            let peer_id = existing_peer.id;
            // Always update name if the peer sent a non-empty one
            if !payload.name.is_empty() && payload.name != existing_peer.name {
                let _ = peer::Entity::update_many()
                    .filter(peer::Column::Id.eq(peer_id))
                    .col_expr(
                        peer::Column::Name,
                        sea_orm::sea_query::Expr::value(payload.name.clone()),
                    )
                    .col_expr(
                        peer::Column::UpdatedAt,
                        sea_orm::sea_query::Expr::value(Utc::now().to_rfc3339()),
                    )
                    .exec(&db)
                    .await;
                tracing::info!(
                    "register_peer: updated peer {} name '{}' -> '{}'",
                    peer_id,
                    existing_peer.name,
                    payload.name
                );
            }

            if key_exchange_done && !existing_peer.key_exchange_done {
                let mut active: peer::ActiveModel = existing_peer.into();
                active.url = Set(payload.url.clone()); // Update URL (port may have changed)
                if payload.library_uuid.is_some() {
                    active.library_uuid = Set(payload.library_uuid.clone());
                }
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
                    // ADR-032: fresh invitation clears any stale-token gate.
                    active.relay_write_token_invalid_at = Set(None);
                }
                active.updated_at = Set(Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
            }
            // If library_uuid changed (peer was reset), update it and clear cached books
            if let Some(new_uuid) = &payload.library_uuid
                && old_uuid.as_deref() != Some(new_uuid.as_str())
            {
                // Update library_uuid on the peer record
                let _ = peer::Entity::update_many()
                    .filter(peer::Column::Id.eq(peer_id))
                    .col_expr(
                        peer::Column::LibraryUuid,
                        sea_orm::sea_query::Expr::value(new_uuid.clone()),
                    )
                    .exec(&db)
                    .await;
                // Clear stale cached books if there was an old uuid
                if old_uuid.is_some() {
                    tracing::info!(
                        "register_peer: peer {} library_uuid changed, clearing cached books",
                        peer_id
                    );
                    let _ = peer_book::Entity::delete_many()
                        .filter(peer_book::Column::PeerId.eq(peer_id))
                        .exec(&db)
                        .await;
                }
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

            let peer_name_for_notif = payload.name.clone();
            let new_peer = peer::ActiveModel {
                name: Set(payload.name),
                url: Set(payload.url),
                library_uuid: Set(payload.library_uuid),
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
                Ok(ref inserted) => {
                    tracing::info!(
                        "Peer: Created new peer '{}' (id={}, e2ee={}, relay={}, status={})",
                        peer_name_for_notif,
                        inserted.id,
                        key_exchange_done,
                        my_relay.is_some(),
                        connection_status
                    );
                    // Signal Flutter instantly so PendingPeersProvider refreshes
                    // without waiting for the 30s fallback timer (direct LAN path has
                    // no relay_poller cycle to emit this automatically).
                    crate::services::nudge_events::bus().emit(
                        crate::services::nudge_events::NudgeEvent {
                            mailbox_id: String::new(),
                            source: crate::services::nudge_events::NudgeSource::Manual,
                        },
                    );

                    if connection_status == "pending" {
                        // Emit connection_request notification (needs user action)
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type: crate::domain::NotificationEventType::ConnectionRequest,
                                title: peer_name_for_notif.clone(),
                                body: None,
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(peer_name_for_notif),
                            },
                        )
                        .await;
                    } else {
                        // Auto-accepted: emit connection_accepted notification
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type:
                                    crate::domain::NotificationEventType::ConnectionAccepted,
                                title: peer_name_for_notif.clone(),
                                body: None,
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(peer_name_for_notif),
                            },
                        )
                        .await;
                    }

                    (
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
                        .into_response()
                }
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

/// Notify a remote peer that we are disconnecting.
///
/// Tries E2EE first (encrypted, with relay fallback for offline peers),
/// then falls back to a plaintext HTTP POST for peers without E2EE keys.
/// Errors are logged but never propagated - disconnection is always local-first.
pub(crate) async fn notify_peer_of_disconnect(
    state: &crate::infrastructure::AppState,
    peer_model: &peer::Model,
) {
    // Send OUR library_uuid (stable identifier) + URL as fallback.
    // The remote peer will search by library_uuid first, then by URL.
    let our_library_uuid = state.identity_service.library_uuid().map(|s| s.to_string());
    let our_url = state.our_public_url();

    let payload = json!({
        "peer_url": our_url,
        "library_uuid": our_library_uuid,
        "timestamp": Utc::now().to_rfc3339(),
    });

    // Try E2EE notification (handles relay fallback internally)
    match try_send_e2ee(state, peer_model, "peer_disconnect", payload).await {
        Ok(Some(_)) => {
            info!(
                "Disconnect notification sent (E2EE) to peer {} ({})",
                peer_model.name, peer_model.id
            );
            return;
        }
        Ok(None) => {
            // E2EE not available for this peer, fall through to plaintext
        }
        Err(e) => {
            info!(
                "E2EE disconnect notification failed for peer {}: {}, trying plaintext",
                peer_model.name, e
            );
        }
    }

    // HMAC-authenticated fallback: POST /api/peers/notify-disconnect
    // Requires key_exchange_done + x25519_public_key (shared secret for HMAC).
    // If keys are not available, we skip entirely (no unauthenticated fallback).
    let our_uuid = match &our_library_uuid {
        Some(uuid) => uuid.clone(),
        None => {
            info!(
                "Disconnect: no library_uuid, skipping plaintext fallback for peer {}",
                peer_model.name
            );
            return;
        }
    };

    if !peer_model.key_exchange_done {
        info!(
            "Disconnect: key_exchange not done, skipping plaintext fallback for peer {}",
            peer_model.name
        );
        return;
    }

    let peer_x25519_hex = match &peer_model.x25519_public_key {
        Some(hex) => hex.clone(),
        None => {
            info!(
                "Disconnect: no x25519_public_key, skipping plaintext fallback for peer {}",
                peer_model.name
            );
            return;
        }
    };

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            info!(
                "Disconnect: CryptoService not initialized, skipping plaintext fallback for peer {}",
                peer_model.name
            );
            return;
        }
    };

    // Compute HMAC
    let timestamp = Utc::now().to_rfc3339();
    let peer_x25519_bytes = match hex::decode(&peer_x25519_hex) {
        Ok(b) if b.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b);
            arr
        }
        _ => {
            info!(
                "Disconnect: invalid x25519_public_key hex for peer {}",
                peer_model.name
            );
            return;
        }
    };
    let peer_pub = x25519_dalek::PublicKey::from(peer_x25519_bytes);
    let hmac = crate::crypto::key_exchange::compute_disconnect_hmac(
        crypto_service.identity().x25519_static_secret(),
        &peer_pub,
        &our_uuid,
        &timestamp,
    );

    let client = get_safe_client();
    let url = format!("{}/api/peers/notify-disconnect", peer_model.url);
    match client
        .post(&url)
        .json(&json!({
            "peer_url": our_url,
            "library_uuid": our_uuid,
            "timestamp": timestamp,
            "hmac": hex::encode(hmac),
        }))
        .send()
        .await
    {
        Ok(res) => {
            info!(
                "Disconnect notification sent (HMAC, status={}) to peer {} ({})",
                res.status(),
                peer_model.name,
                peer_model.id
            );
        }
        Err(e) => {
            info!(
                "HMAC disconnect notification failed for peer {}: {} (peer may be offline)",
                peer_model.name, e
            );
        }
    }
}

/// Receive an HMAC-authenticated disconnect notification from a remote peer.
///
/// Defense layers:
/// 1. Requires library_uuid, timestamp, and HMAC (all mandatory)
/// 2. Validates timestamp within +/-5 minutes (replay window)
/// 3. Verifies HMAC using X25519 static shared secret
/// 4. Re-handshake: asks the sender to confirm the disconnect
pub async fn receive_disconnect_notification(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<DisconnectNotification>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Require all authentication fields
    let library_uuid = match &payload.library_uuid {
        Some(uuid) if !uuid.trim().is_empty() => uuid.trim().to_string(),
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing library_uuid" })),
            )
                .into_response();
        }
    };
    let timestamp = match &payload.timestamp {
        Some(ts) if !ts.trim().is_empty() => ts.trim().to_string(),
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing timestamp" })),
            )
                .into_response();
        }
    };
    let hmac_hex = match &payload.hmac {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing HMAC" })),
            )
                .into_response();
        }
    };

    // 2. Validate timestamp (must be within +/-5 minutes)
    let parsed_ts = match chrono::DateTime::parse_from_rfc3339(&timestamp) {
        Ok(ts) => ts.with_timezone(&Utc),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid timestamp format (expected RFC3339)" })),
            )
                .into_response();
        }
    };
    let now = Utc::now();
    let drift = (now - parsed_ts).abs();
    if drift > chrono::Duration::minutes(5) {
        return (
            StatusCode::GONE,
            Json(json!({ "error": "Timestamp outside acceptable window" })),
        )
            .into_response();
    }

    // 3. Decode HMAC hex
    let hmac_bytes = match hex::decode(&hmac_hex) {
        Ok(b) if b.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b);
            arr
        }
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid HMAC format" })),
            )
                .into_response();
        }
    };

    // 4. Find peer by library_uuid first, then URL fallback
    let peer_url = payload.peer_url.trim();
    let found_peer = match peer::Entity::find()
        .filter(peer::Column::LibraryUuid.eq(library_uuid.as_str()))
        .one(db)
        .await
    {
        Ok(Some(p)) => Some(p),
        Ok(None) => {
            if !peer_url.is_empty() {
                peer::Entity::find()
                    .filter(peer::Column::Url.eq(peer_url))
                    .one(db)
                    .await
                    .ok()
                    .flatten()
            } else {
                None
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

    let peer_model = match found_peer {
        Some(p) => p,
        None => {
            return (
                StatusCode::OK,
                Json(json!({ "message": "Peer not found, already disconnected" })),
            )
                .into_response();
        }
    };

    // 5. Verify HMAC - requires key_exchange_done + x25519_public_key
    if !peer_model.key_exchange_done {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Key exchange not completed with this peer" })),
        )
            .into_response();
    }

    let peer_x25519_bytes = match &peer_model.x25519_public_key {
        Some(hex_str) => match hex::decode(hex_str) {
            Ok(b) if b.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&b);
                arr
            }
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "Invalid peer x25519 key" })),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Peer missing x25519_public_key" })),
            )
                .into_response();
        }
    };

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Crypto service not initialized" })),
            )
                .into_response();
        }
    };

    let peer_pub = x25519_dalek::PublicKey::from(peer_x25519_bytes);
    let valid = crate::crypto::key_exchange::verify_disconnect_hmac(
        crypto_service.identity().x25519_static_secret(),
        &peer_pub,
        &library_uuid,
        &timestamp,
        &hmac_bytes,
    );

    if !valid {
        tracing::warn!(
            "Disconnect HMAC verification failed for library_uuid={}",
            library_uuid
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "HMAC verification failed" })),
        )
            .into_response();
    }

    // 6. Re-handshake: confirm with the sender that they really disconnected
    let our_library_uuid = state
        .identity_service
        .library_uuid()
        .map(|s| s.to_string())
        .unwrap_or_default();

    match verify_disconnect_with_peer(&peer_model.url, &our_library_uuid).await {
        Some(false) => {
            tracing::warn!(
                "Re-handshake: peer {} denied disconnect (spoofed notification)",
                peer_model.name
            );
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "Peer denied the disconnect" })),
            )
                .into_response();
        }
        Some(true) | None => {
            // Confirmed or unreachable (timeout) - proceed with deletion
        }
    }

    // 7. Delete the peer
    let peer_name = peer_model.name.clone();
    let peer_id = peer_model.id;
    match peer::Entity::delete_by_id(peer_id).exec(db).await {
        Ok(_) => {
            info!(
                "Peer {} ({}) removed via authenticated disconnect (uuid={})",
                peer_name, peer_id, library_uuid
            );
            (
                StatusCode::OK,
                Json(json!({ "message": "Disconnect acknowledged" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct DisconnectNotification {
    pub peer_url: String,
    /// Stable library UUID - used as primary lookup for peer identification.
    pub library_uuid: Option<String>,
    /// RFC3339 timestamp of the disconnect event (required for HMAC verification).
    pub timestamp: Option<String>,
    /// Hex-encoded 32-byte HMAC (required for authentication).
    pub hmac: Option<String>,
}

/// Request body for the re-handshake confirmation endpoint.
#[derive(Debug, Deserialize)]
pub struct VerifyDisconnectRequest {
    /// The library_uuid of the peer asking for confirmation.
    pub library_uuid: String,
}

/// Re-handshake endpoint: a peer asks us "did you really disconnect from me?"
///
/// Returns `confirmed: true` if we no longer have this peer in our database
/// (meaning we did initiate a disconnect). Returns `confirmed: false` if the
/// peer still exists (the disconnect was likely spoofed).
pub async fn verify_disconnect(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<VerifyDisconnectRequest>,
) -> impl IntoResponse {
    let db = state.db();
    let uuid = req.library_uuid.trim();
    if uuid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing library_uuid" })),
        )
            .into_response();
    }

    // If the peer is NOT in our database, we confirm the disconnect
    let still_exists = peer::Entity::find()
        .filter(peer::Column::LibraryUuid.eq(uuid))
        .count(db)
        .await
        .unwrap_or(0)
        > 0;

    let confirmed = !still_exists;
    (StatusCode::OK, Json(json!({ "confirmed": confirmed }))).into_response()
}

/// Ask a remote peer to confirm that they really initiated a disconnect.
///
/// Returns:
/// - `Some(true)`: peer confirms (they no longer have us)
/// - `Some(false)`: peer denies (they still have us - disconnect was spoofed)
/// - `None`: peer unreachable (timeout, network error)
pub(crate) async fn verify_disconnect_with_peer(
    peer_url: &str,
    our_library_uuid: &str,
) -> Option<bool> {
    if let Err(e) = validate_url(peer_url) {
        tracing::warn!("verify_disconnect: invalid peer URL {}: {}", peer_url, e);
        return None;
    }

    let client = get_safe_client();
    let url = format!("{}/api/peers/verify-disconnect", peer_url);

    match client
        .post(&url)
        .json(&json!({ "library_uuid": our_library_uuid }))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => {
            if let Ok(body) = res.json::<serde_json::Value>().await {
                body.get("confirmed").and_then(|v| v.as_bool())
            } else {
                None
            }
        }
        Ok(res) => {
            tracing::info!(
                "verify_disconnect: peer {} returned status {}",
                peer_url,
                res.status()
            );
            None
        }
        Err(e) => {
            tracing::info!("verify_disconnect: peer {} unreachable: {}", peer_url, e);
            None
        }
    }
}
