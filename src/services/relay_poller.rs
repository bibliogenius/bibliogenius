//! Background relay poller — periodically checks the relay hub for incoming messages.
//!
//! See SECURITY_GUIDELINES.md §B10 for polling jitter requirements.
//! ADR-012: Added reply-to deposit logic and correlation matching for relay request-response.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

use tokio::sync::RwLock;

use crate::api::e2ee::{build_known_peers, dispatch_clear_message};

/// Cooldown tracker for relay-based peer views (keyed by peer ID).
/// 15-minute cooldown per peer, same as the HTTP middleware.
static RELAY_VIEW_COOLDOWN: std::sync::LazyLock<RwLock<HashMap<i32, Instant>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Count a relay-based library view from a peer, with 15-min cooldown per peer ID.
async fn count_relay_view(db: &sea_orm::DatabaseConnection, peer_id: i32) {
    tracing::debug!("View counter: counting relay view for peer {peer_id}");
    let cooldown = Duration::from_secs(900);
    let now = Instant::now();

    // Fast path: read-only check
    {
        let map = RELAY_VIEW_COOLDOWN.read().await;
        if let Some(&last) = map.get(&peer_id)
            && now.duration_since(last) < cooldown
        {
            tracing::debug!("View counter: peer {peer_id} in cooldown, skipping");
            return;
        }
    }

    // Slow path: write lock
    let mut map = RELAY_VIEW_COOLDOWN.write().await;
    if let Some(&last) = map.get(&peer_id)
        && now.duration_since(last) < cooldown
    {
        return;
    }
    map.insert(peer_id, now);
    if map.len() > 100 {
        map.retain(|_, last| now.duration_since(*last) < cooldown);
    }
    drop(map);

    tracing::debug!("View counter: recording peer view in DB");
    crate::api::view_counter::record_peer_view(db).await;
}

/// Fetch follower view count from the hub and sync it to local SQLite.
/// Called periodically (~every 30 min) from the polling loop.
async fn sync_follower_views(db: &sea_orm::DatabaseConnection) {
    use crate::services::hub_directory_service::HubDirectoryService;

    // 1. Get our hub directory config (contains our node_id)
    let config = match HubDirectoryService::get_config(db).await {
        Ok(Some(c)) => c,
        _ => return, // Not registered on hub, nothing to sync
    };

    // 2. Fetch our own profile from the hub (includes view_count)
    let svc = HubDirectoryService::new();
    match svc.get_profile(&config.node_id).await {
        Ok(profile) => {
            if let Some(count) = profile.view_count
                && let Err(e) = crate::api::view_counter::record_follower_views(db, count).await
            {
                tracing::warn!("Failed to record follower views: {e}");
            }
        }
        Err(e) => {
            tracing::debug!("Follower views sync skipped: {e:?}");
        }
    }
}

use crate::api::relay::get_my_relay_config;
use crate::crypto::envelope::ClearMessage;
use crate::infrastructure::AppState;
use crate::models::peer;
use crate::services::e2ee_transport::E2eeTransportError;
use crate::services::relay_transport::{RelayBlob, RelayTransport};

/// Start the background relay polling loop.
///
/// Polls at `interval` + random jitter (0-10s per B10) for incoming messages.
/// Each message is opened, dispatched through the standard E2EE pipeline,
/// then acknowledged (deleted from the relay).
pub async fn start_relay_polling(state: AppState, interval: Duration) {
    use rand::Rng;

    tracing::info!("Relay poller: started (interval: {}s)", interval.as_secs());

    // First poll immediately at startup (auto-heal stale mailboxes without waiting 60s)
    if let Err(e) = poll_once(&state).await {
        tracing::warn!("Relay poller: {e}");
    }

    let mut poll_count: u32 = 0;

    loop {
        // Jitter: 0-10 seconds (B10: prevent timing correlation)
        let jitter_ms = rand::thread_rng().gen_range(0..10_000);
        tokio::time::sleep(interval + Duration::from_millis(jitter_ms)).await;

        if let Err(e) = poll_once(&state).await {
            tracing::warn!("Relay poller: {e}");
        }

        // Sync follower views from hub every ~30 polls (~30 min at 60s interval)
        poll_count += 1;
        if poll_count.is_multiple_of(30) {
            sync_follower_views(state.db()).await;
        }
    }
}

/// Execute a single poll cycle. Public so it can be triggered by `poll_now` endpoint (ADR-012).
pub async fn poll_once(state: &AppState) -> Result<(), String> {
    let db = state.db();

    // 1. Load my relay config
    let config = match get_my_relay_config(db).await {
        Some(c) => c,
        None => {
            tracing::debug!("Relay poller: No relay config, skipping");
            return Ok(());
        }
    };

    // 2. Get crypto service (optional - only needed for encrypted messages)
    let crypto_service = state.crypto_service().cloned();

    // 3. Poll relay for pending messages (does not require crypto)
    let relay = RelayTransport::new(crypto_service.clone());
    let (envelopes, raw_blobs) = match relay
        .poll(&config.relay_url, &config.mailbox_uuid, &config.read_token)
        .await
    {
        Ok(result) => result,
        Err(E2eeTransportError::PeerError(404, _)) => {
            // Mailbox expired or deleted on the hub - auto-recreate
            tracing::warn!(
                "Relay poller: Mailbox {} not found (expired/deleted), recreating...",
                config.mailbox_uuid
            );
            match recreate_mailbox(db, &config.relay_url).await {
                Ok(new_uuid) => {
                    tracing::info!("Relay poller: New mailbox created: {new_uuid}");
                    // Notify peers of new credentials (requires crypto, handled internally)
                    notify_peers_of_new_credentials(state, &config.relay_url, &new_uuid).await;
                }
                Err(e) => {
                    tracing::warn!("Relay poller: Failed to recreate mailbox: {e}");
                }
            }
            return Ok(());
        }
        Err(e) => return Err(format!("poll failed: {e}")),
    };

    if envelopes.is_empty() && raw_blobs.is_empty() {
        tracing::debug!(
            "Relay poller: No messages in mailbox {}",
            config.mailbox_uuid
        );
        return Ok(());
    }

    tracing::info!(
        "Relay poller: Received {} encrypted + {} raw message(s) from relay",
        envelopes.len(),
        raw_blobs.len()
    );

    // 4a. Process raw messages first (e.g., connection requests from new peers).
    // These do NOT require crypto and must always be processed.
    for blob in &raw_blobs {
        let RelayBlob::Raw(msg_id, bytes) = blob else {
            continue;
        };
        if let Err(e) = handle_raw_relay_message(db, bytes).await {
            tracing::warn!("Relay poller: Failed to process raw message {msg_id}: {e}");
        }
        // Always ACK raw messages to prevent mailbox bloat.
        // Failed messages are not retryable (malformed data won't fix itself).
        if let Err(e) = relay
            .ack(
                &config.relay_url,
                &config.mailbox_uuid,
                &config.read_token,
                *msg_id,
            )
            .await
        {
            tracing::warn!("Relay poller: Failed to ack raw message {msg_id}: {e}");
        }
    }

    // 4b. Encrypted messages require the crypto service.
    // If crypto is not ready, leave encrypted messages in the mailbox for next cycle.
    let Some(crypto_svc) = crypto_service else {
        if !envelopes.is_empty() {
            tracing::warn!(
                "Relay poller: {} encrypted message(s) pending but crypto not ready - will retry next cycle",
                envelopes.len()
            );
        }
        return Ok(());
    };

    // 4c. Load all E2EE-capable peers (reload after raw processing, new peers may exist)
    let peers = peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .all(db)
        .await
        .map_err(|e| format!("failed to load peers: {e}"))?;

    let (known_peers, peer_models) = build_known_peers(&peers);
    if known_peers.is_empty() && !envelopes.is_empty() {
        tracing::warn!("Relay poller: No known E2EE peers, cannot process encrypted messages");
        return Ok(());
    }

    // 5. Process each encrypted message
    for (message_id, envelope) in &envelopes {
        match process_relay_message(
            state,
            &config.relay_url,
            &crypto_svc,
            envelope,
            &known_peers,
            &peer_models,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("Relay poller: Failed to process message {message_id}: {e}");
            }
        }
        // Always ACK to prevent mailbox bloat. E2EE messages that fail to
        // decrypt (wrong key, unknown sender) won't succeed on retry.
        if let Err(e) = relay
            .ack(
                &config.relay_url,
                &config.mailbox_uuid,
                &config.read_token,
                *message_id,
            )
            .await
        {
            tracing::warn!("Relay poller: Failed to ack message {message_id}: {e}");
        }
    }

    Ok(())
}

/// Message types that produce a response payload (request-response pattern).
const REQUEST_RESPONSE_TYPES: &[&str] = &[
    "book_sync_request",
    "search_request",
    "device_sync_request",
    "library_manifest_request",
    "library_page_request",
    "library_search_request",
    "loan_request",
];

/// Response message types (correlation targets, ADR-012).
const RESPONSE_TYPES: &[&str] = &[
    "library_manifest_response",
    "library_page_response",
    "library_search_response",
];

/// Process a single relay message through the existing E2EE pipeline.
///
/// ADR-012 extensions:
/// - If the message is a response with `correlation_id`, resolve the pending request.
/// - If the message is a request with `reply_to_*` fields, compute the response
///   and deposit it in the requester's mailbox.
async fn process_relay_message(
    state: &AppState,
    relay_url: &str,
    crypto_service: &Arc<
        crate::services::crypto_service::CryptoService<
            crate::infrastructure::nonce_store::SqliteNonceStore,
        >,
    >,
    envelope: &crate::crypto::envelope::EncryptedEnvelope,
    known_peers: &[crate::services::crypto_service::PeerInfo],
    peer_models: &[peer::Model],
) -> Result<(), String> {
    let db = state.db();

    // Open the envelope
    let (clear_message, peer_index) = crypto_service
        .open(envelope, known_peers)
        .map_err(|e| format!("failed to open envelope: {e}"))?;

    let sender_peer = &peer_models[peer_index];
    tracing::info!(
        "Relay poller: Received '{}' from peer {} ({})",
        clear_message.message_type,
        sender_peer.name,
        sender_peer.id
    );

    // Handle relay credential updates (peer recreated their mailbox)
    if clear_message.message_type == "relay_credential_update" {
        return handle_credential_update(db, sender_peer, &clear_message).await;
    }

    // ADR-012: Check if this is a response with a correlation_id
    if RESPONSE_TYPES.contains(&clear_message.message_type.as_str()) {
        if let Some(ref corr_id) = clear_message.correlation_id {
            if state.resolve_relay_request(corr_id, clear_message.payload.clone()) {
                tracing::info!(
                    "Relay poller: Resolved correlation {} for '{}'",
                    corr_id,
                    clear_message.message_type
                );
                return Ok(());
            }
            tracing::debug!(
                "Relay poller: No pending listener for correlation {} (message type: {})",
                corr_id,
                clear_message.message_type
            );
        }
        // Response without correlation or without a listener - just ack it
        return Ok(());
    }

    // ADR-012: For request-response types with reply_to fields,
    // compute the response and deposit it in the requester's mailbox.
    let has_reply_to =
        clear_message.reply_to_mailbox.is_some() && clear_message.reply_to_write_token.is_some();

    if has_reply_to && REQUEST_RESPONSE_TYPES.contains(&clear_message.message_type.as_str()) {
        return handle_relay_request_response(
            state,
            relay_url,
            crypto_service,
            &clear_message,
            known_peers,
            peer_index,
            sender_peer,
        )
        .await;
    }

    // Standard dispatch (fire-and-forget messages, or request-response without reply_to)
    let our_uuid = state.identity_service.library_uuid().map(|s| s.to_string());
    let response = dispatch_clear_message(
        db,
        crypto_service,
        &clear_message,
        known_peers,
        peer_index,
        sender_peer,
        our_uuid.as_deref(),
    )
    .await;

    if response.status().is_server_error() {
        return Err(format!(
            "handler returned {} for '{}' from peer {}",
            response.status(),
            clear_message.message_type,
            sender_peer.name
        ));
    }

    Ok(())
}

/// Handle a request-response message that arrived via relay with reply_to fields (ADR-012).
///
/// 1. Compute the response payload using the appropriate handler
/// 2. Seal and deposit the response in the requester's mailbox
async fn handle_relay_request_response(
    state: &AppState,
    relay_url: &str,
    crypto_service: &Arc<
        crate::services::crypto_service::CryptoService<
            crate::infrastructure::nonce_store::SqliteNonceStore,
        >,
    >,
    clear_message: &ClearMessage,
    known_peers: &[crate::services::crypto_service::PeerInfo],
    peer_index: usize,
    sender_peer: &peer::Model,
) -> Result<(), String> {
    let db = state.db();
    let reply_to_mailbox = clear_message.reply_to_mailbox.as_ref().unwrap();
    let reply_to_write_token = clear_message.reply_to_write_token.as_ref().unwrap();

    // Determine response type and compute payload
    let (response_type, response_payload) = match clear_message.message_type.as_str() {
        "library_manifest_request" => (
            "library_manifest_response",
            crate::api::e2ee::handle_library_manifest_request(db).await,
        ),
        "library_page_request" => (
            "library_page_response",
            crate::api::e2ee::handle_library_page_request(db, clear_message).await,
        ),
        "library_search_request" => (
            "library_search_response",
            crate::api::e2ee::handle_library_search_via_relay(db, clear_message).await,
        ),
        "book_sync_request" => (
            "book_sync_response",
            crate::api::e2ee::handle_book_sync_request(db).await,
        ),
        "search_request" => (
            "search_response",
            crate::api::e2ee::handle_search_request(db, clear_message).await,
        ),
        "loan_request" => (
            "loan_request_response",
            crate::api::e2ee::handle_loan_request_for_relay(db, sender_peer, clear_message).await,
        ),
        "request_status_query" => (
            "request_status_response",
            crate::api::e2ee::handle_request_status_query(db, clear_message).await,
        ),
        _ => {
            // For other request-response types, fall back to standard dispatch
            let our_uuid = state.identity_service.library_uuid().map(|s| s.to_string());
            let response = dispatch_clear_message(
                db,
                crypto_service,
                clear_message,
                known_peers,
                peer_index,
                sender_peer,
                our_uuid.as_deref(),
            )
            .await;
            if response.status().is_server_error() {
                return Err(format!(
                    "handler returned {} for '{}' from peer {}",
                    response.status(),
                    clear_message.message_type,
                    sender_peer.name
                ));
            }
            return Ok(());
        }
    };

    // Count relay-based library views (browsing/search) with per-peer cooldown
    if matches!(
        clear_message.message_type.as_str(),
        "library_manifest_request" | "library_page_request" | "library_search_request"
    ) {
        count_relay_view(db, sender_peer.id).await;
    }

    // Build response ClearMessage with correlation_id from the original request
    let response_msg = ClearMessage {
        message_type: response_type.to_string(),
        payload: response_payload,
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: clear_message.correlation_id.clone(),
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    // Deposit encrypted response in requester's mailbox
    let relay = RelayTransport::new(Some(crypto_service.clone()));
    relay
        .deposit_response(
            relay_url,
            reply_to_mailbox,
            reply_to_write_token,
            &known_peers[peer_index].x25519_public,
            &response_msg,
        )
        .await
        .map_err(|e| format!("failed to deposit relay response: {e}"))?;

    tracing::info!(
        "Relay poller: Deposited '{}' response for peer {} (correlation: {:?})",
        response_type,
        sender_peer.name,
        clear_message.correlation_id
    );

    Ok(())
}

/// Recreate a relay mailbox on the hub when the existing one has expired or been deleted.
///
/// Creates a new mailbox via the hub API and updates `my_relay_config` in the local database.
/// Peers will learn the new credentials on their next sync (via `/api/config`).
pub async fn recreate_mailbox(
    db: &sea_orm::DatabaseConnection,
    relay_url: &str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

    let url = format!("{}/api/relay/mailbox", relay_url.trim_end_matches('/'));

    let response = client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to reach relay hub: {e}"))?;

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Relay hub error: {body}"));
    }

    let result: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Invalid relay response: {e}"))?;

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
        return Err("Relay hub returned incomplete mailbox data".to_string());
    }

    // Update my_relay_config (replace singleton row)
    let _ = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM my_relay_config".to_owned(),
        ))
        .await;

    let config = crate::models::relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(relay_url.to_string()),
        mailbox_uuid: Set(mailbox_uuid.clone()),
        read_token: Set(read_token),
        write_token: Set(write_token),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    config
        .insert(db)
        .await
        .map_err(|e| format!("Failed to save relay config: {e}"))?;

    tracing::info!("Relay: Mailbox recreated successfully: {mailbox_uuid}");
    Ok(mailbox_uuid)
}

/// Handle a `relay_credential_update` message from a peer who recreated their mailbox.
///
/// Updates the peer's relay credentials in our database so future relay sends
/// use the correct mailbox UUID and write token.
async fn handle_credential_update(
    db: &sea_orm::DatabaseConnection,
    sender_peer: &peer::Model,
    message: &ClearMessage,
) -> Result<(), String> {
    let payload = &message.payload;
    let new_relay_url = payload
        .get("relay_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new_mailbox_id = payload
        .get("mailbox_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new_write_token = payload
        .get("write_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if new_relay_url.is_empty() || new_mailbox_id.is_empty() || new_write_token.is_empty() {
        return Err("relay_credential_update: incomplete payload".to_string());
    }

    if let Ok(Some(existing)) = peer::Entity::find_by_id(sender_peer.id).one(db).await {
        let mut active: peer::ActiveModel = existing.into();
        active.relay_url = Set(Some(new_relay_url.to_string()));
        active.mailbox_id = Set(Some(new_mailbox_id.to_string()));
        active.relay_write_token = Set(Some(new_write_token.to_string()));
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active
            .update(db)
            .await
            .map_err(|e| format!("Failed to update peer relay credentials: {e}"))?;
        tracing::info!(
            "Relay: Updated credentials for peer {} (new mailbox: {})",
            sender_peer.name,
            new_mailbox_id
        );
    }

    Ok(())
}

/// Handle a raw (non-E2EE) relay message.
///
/// Currently supports `connection_request`: a new peer sends their info via
/// the relay because they accepted our invite link without WiFi (no direct
/// HTTP handshake possible). We save them as a peer so future E2EE
/// communication can work.
///
/// Security: the message is not encrypted, but it was deposited using
/// our mailbox write_token (shared via the invite link). Anyone with the
/// invite can deposit a connection request - this is by design (the invite
/// is the trust anchor).
async fn handle_raw_relay_message(
    db: &sea_orm::DatabaseConnection,
    bytes: &[u8],
) -> Result<(), String> {
    // Try to parse as JSON
    let json: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("not valid JSON: {e}"))?;

    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "connection_request" => handle_connection_request(db, &json).await,
        _ => {
            tracing::warn!(
                "Relay poller: Unknown raw message type '{}', discarding",
                msg_type
            );
            // Acknowledge unknown messages to avoid infinite retry
            Ok(())
        }
    }
}

/// Process a `connection_request` raw relay message.
///
/// Creates or updates the sender as a peer with their E2EE keys and relay
/// credentials so that future encrypted communication can work.
async fn handle_connection_request(
    db: &sea_orm::DatabaseConnection,
    json: &serde_json::Value,
) -> Result<(), String> {
    let name = json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown Library");
    let url = json.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let ed25519_key = json
        .get("ed25519_public_key")
        .and_then(|v| v.as_str())
        .map(String::from);
    let x25519_key = json
        .get("x25519_public_key")
        .and_then(|v| v.as_str())
        .map(String::from);
    let relay_url = json
        .get("relay_url")
        .and_then(|v| v.as_str())
        .map(String::from);
    let mailbox_id = json
        .get("mailbox_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let relay_write_token = json
        .get("relay_write_token")
        .and_then(|v| v.as_str())
        .map(String::from);

    // URL may be empty when the sender has no WiFi (relay-only connection).
    // Use a unique placeholder to satisfy the NOT NULL UNIQUE constraint on peers.url.
    // It will be replaced with the real URL on the first direct WiFi sync.
    let peer_url = if url.is_empty() {
        let unique_part = ed25519_key
            .as_deref()
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        format!("relay://{unique_part}")
    } else {
        url.to_string()
    };

    let key_exchange_done = ed25519_key.is_some() && x25519_key.is_some();

    tracing::info!(
        "Relay poller: Received connection_request from '{}' (url: {}, e2ee: {})",
        name,
        peer_url,
        key_exchange_done
    );

    // Check if peer already exists (by ed25519 key first, then by URL if non-empty)
    let existing_by_key = if let Some(ref key) = ed25519_key {
        peer::Entity::find()
            .filter(peer::Column::PublicKey.eq(key.as_str()))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let existing_by_url = if existing_by_key.is_none() && !peer_url.starts_with("relay://") {
        peer::Entity::find()
            .filter(peer::Column::Url.eq(&peer_url))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let existing = existing_by_key.or(existing_by_url);

    if let Some(existing_peer) = existing {
        // Update existing peer with new keys/credentials
        let mut active: peer::ActiveModel = existing_peer.into();
        active.name = Set(name.to_string());
        if ed25519_key.is_some() {
            active.public_key = Set(ed25519_key);
        }
        if x25519_key.is_some() {
            active.x25519_public_key = Set(x25519_key);
        }
        active.key_exchange_done = Set(key_exchange_done);
        if relay_url.is_some() {
            active.relay_url = Set(relay_url);
        }
        if mailbox_id.is_some() {
            active.mailbox_id = Set(mailbox_id);
        }
        if relay_write_token.is_some() {
            active.relay_write_token = Set(relay_write_token);
        }
        active.connection_status = Set("accepted".to_string());
        active.auto_approve = Set(true);
        active.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active
            .update(db)
            .await
            .map_err(|e| format!("Failed to update peer from connection_request: {e}"))?;

        tracing::info!(
            "Relay poller: Updated existing peer '{}' from connection_request",
            name
        );
    } else {
        // Insert new peer
        let new_peer = peer::ActiveModel {
            name: Set(name.to_string()),
            url: Set(peer_url),
            public_key: Set(ed25519_key),
            x25519_public_key: Set(x25519_key),
            key_exchange_done: Set(key_exchange_done),
            relay_url: Set(relay_url),
            mailbox_id: Set(mailbox_id),
            relay_write_token: Set(relay_write_token),
            connection_status: Set("accepted".to_string()),
            auto_approve: Set(true),
            last_seen: Set(Some(chrono::Utc::now().to_rfc3339())),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        peer::Entity::insert(new_peer)
            .exec(db)
            .await
            .map_err(|e| format!("Failed to insert peer from connection_request: {e}"))?;

        tracing::info!(
            "Relay poller: Created new peer '{}' from connection_request",
            name
        );
    }

    Ok(())
}

/// After recreating our mailbox, notify all E2EE peers of the new credentials.
///
/// Deposits a `relay_credential_update` message in each peer's relay mailbox.
/// If a peer's mailbox is also expired (404), the notification silently fails
/// and credentials will be exchanged on the next direct sync.
async fn notify_peers_of_new_credentials(state: &AppState, relay_url: &str, new_mailbox: &str) {
    let db = state.db();

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => return,
    };

    // Load our fresh relay config (just created)
    let my_config = match get_my_relay_config(db).await {
        Some(c) => c,
        None => return,
    };

    // Load E2EE peers that have relay credentials
    let peers = match peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .all(db)
        .await
    {
        Ok(p) => p,
        Err(_) => return,
    };

    let credential_payload = serde_json::json!({
        "relay_url": relay_url,
        "mailbox_id": new_mailbox,
        "write_token": my_config.write_token,
    });

    let message = ClearMessage {
        message_type: "relay_credential_update".to_string(),
        payload: credential_payload,
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let relay = RelayTransport::new(Some(crypto_service));

    for p in &peers {
        let (Some(peer_relay_url), Some(peer_mailbox), Some(peer_write_token)) =
            (&p.relay_url, &p.mailbox_id, &p.relay_write_token)
        else {
            continue;
        };

        // Parse peer's X25519 key for encryption
        let Some(x25519_hex) = &p.x25519_public_key else {
            continue;
        };
        let Ok(x_bytes) = hex::decode(x25519_hex) else {
            continue;
        };
        if x_bytes.len() != 32 {
            continue;
        }
        let x_arr: [u8; 32] = x_bytes.try_into().unwrap();
        let peer_x25519 = x25519_dalek::PublicKey::from(x_arr);

        match relay
            .send(
                peer_relay_url,
                peer_mailbox,
                peer_write_token,
                &peer_x25519,
                &message,
            )
            .await
        {
            Ok(()) => {
                tracing::info!("Relay: Notified peer {} of new relay credentials", p.name);
            }
            Err(E2eeTransportError::PeerError(404, _)) => {
                // Peer's mailbox also expired - credentials will sync later via WiFi
                tracing::info!(
                    "Relay: Peer {} mailbox also expired, credential sync deferred",
                    p.name
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Relay: Failed to notify peer {} of new credentials: {e}",
                    p.name
                );
            }
        }
    }
}
