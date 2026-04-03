#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
use crate::models::{operation_log, peer, peer_book, peer_gamification_stats};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use futures::future::join_all;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, Set,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info};
use url::Url;

/// Validate URL to prevent SSRF (OWASP A10).
///
/// Blocks:
/// - Non-HTTP/HTTPS schemes (file://, ftp://, javascript:, etc.)
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-local (169.254.0.0/16, fe80::/10) - includes AWS metadata 169.254.169.254
/// - Multicast (224.0.0.0/4, ff00::/8)
/// - Unspecified (0.0.0.0, ::)
/// - Broadcast (255.255.255.255)
/// - "localhost" hostname
///
/// Allows:
/// - Private networks (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) for P2P LAN use
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
        Some(url::Host::Ipv4(ip)) => {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            // Link-local: 169.254.0.0/16 (includes AWS metadata endpoint 169.254.169.254)
            let octets = ip.octets();
            if octets[0] == 169 && octets[1] == 254 {
                return Err("Link-local addresses blocked".to_string());
            }
            if ip.is_multicast() {
                return Err("Multicast addresses blocked".to_string());
            }
            if ip.is_unspecified() {
                return Err("Unspecified address blocked".to_string());
            }
            // Broadcast: 255.255.255.255
            if octets == [255, 255, 255, 255] {
                return Err("Broadcast address blocked".to_string());
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            if ip.is_multicast() {
                return Err("Multicast addresses blocked".to_string());
            }
            if ip.is_unspecified() {
                return Err("Unspecified address blocked".to_string());
            }
            // IPv6 link-local: fe80::/10
            let segments = ip.segments();
            if (segments[0] & 0xffc0) == 0xfe80 {
                return Err("Link-local addresses blocked".to_string());
            }
        }
        None => {
            return Err("URL must have a host".to_string());
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
pub(crate) async fn is_auto_approve_loans_enabled(db: &DatabaseConnection) -> bool {
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

/// Result of a successful loan acceptance on the lender side.
pub(crate) struct LoanAcceptResult {
    pub lender_name: String,
    pub due_date: String,
    pub book_isbn: Option<String>,
    pub book_title: String,
    pub book_cover_url: Option<String>,
}

/// Resolve the effective loan duration (in days) for a given book.
///
/// Reads from `loan_settings` (global default + per-book override).
/// Falls back to 21 days if the settings table is unreachable.
async fn resolve_loan_duration_days(db: &DatabaseConnection, book_id: i32) -> i64 {
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;
    match repo.get_effective_duration(book_id).await {
        Ok(days) => days as i64,
        Err(e) => {
            tracing::warn!("Failed to read loan settings, using 21-day default: {e}");
            21
        }
    }
}

/// Core acceptance logic shared by plaintext and E2EE auto-approve paths.
///
/// Finds book/copy, creates contact/loan, updates copy status and request status.
/// Does NOT send notifications to the borrower (caller handles that).
pub(crate) async fn perform_loan_acceptance(
    db: &DatabaseConnection,
    request_id: &str,
    book_isbn: &str,
    book_title: &str,
    peer: &peer::Model,
) -> Result<LoanAcceptResult, String> {
    use crate::models::{book, contact, copy, loan, p2p_request};

    // 1. Find Book by ISBN (fallback to title)
    let book = match book::Entity::find()
        .filter(book::Column::Isbn.eq(book_isbn))
        .one(db)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => match book::Entity::find()
            .filter(book::Column::Title.eq(book_title))
            .one(db)
            .await
        {
            Ok(Some(b)) => b,
            _ => {
                return Err(format!(
                    "Book not found (ISBN: '{book_isbn}', Title: '{book_title}')"
                ));
            }
        },
        Err(e) => return Err(format!("DB error finding book: {e}")),
    };

    // 2. Find available copy
    let copy = match copy::Entity::find()
        .filter(copy::Column::BookId.eq(book.id))
        .filter(copy::Column::Status.eq("available"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        _ => return Err("No available copies".to_string()),
    };

    // 3. Find or create contact for peer
    let contact = match contact::Entity::find()
        .filter(contact::Column::Name.eq(&peer.name))
        .filter(contact::Column::Type.eq("Library"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            let lib_id = crate::utils::library_helpers::resolve_library_id(db)
                .await
                .map_err(|e| format!("No library: {e}"))?;
            let new_contact = contact::ActiveModel {
                r#type: Set("Library".to_string()),
                name: Set(peer.name.clone()),
                library_owner_id: Set(lib_id),
                is_active: Set(true),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            new_contact
                .insert(db)
                .await
                .map_err(|e| format!("Failed to create contact: {e}"))?
        }
        Err(e) => return Err(format!("DB error finding contact: {e}")),
    };

    // 4. Create loan
    let lib_id = crate::utils::library_helpers::resolve_library_id(db)
        .await
        .map_err(|e| format!("No library: {e}"))?;
    let duration_days = resolve_loan_duration_days(db, book.id).await;
    let due = Utc::now() + chrono::Duration::days(duration_days);
    let loan = loan::ActiveModel {
        copy_id: Set(copy.id),
        contact_id: Set(contact.id),
        library_id: Set(lib_id),
        loan_date: Set(Utc::now().to_rfc3339()),
        due_date: Set(due.to_rfc3339()),
        status: Set("active".to_string()),
        created_at: Set(Utc::now().to_rfc3339()),
        updated_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    loan::Entity::insert(loan)
        .exec(db)
        .await
        .map_err(|e| format!("Failed to create loan: {e}"))?;

    // 5. Update copy status
    info!("Auto-approve: Updating copy {} status to 'loaned'", copy.id);
    let mut active_copy: copy::ActiveModel = copy.into();
    active_copy.status = Set("loaned".to_string());
    active_copy
        .update(db)
        .await
        .map_err(|e| format!("Failed to update copy status: {e}"))?;

    // 6. Update request status to accepted
    if let Ok(Some(req)) = p2p_request::Entity::find_by_id(request_id).one(db).await {
        let mut active_req: p2p_request::ActiveModel = req.into();
        active_req.status = Set("accepted".to_string());
        active_req.updated_at = Set(Utc::now().to_rfc3339());
        let _ = active_req.update(db).await;
    }

    // 7. Get lender name
    let lender_name = match crate::models::library::Entity::find_by_id(1).one(db).await {
        Ok(Some(lib)) => lib.name,
        _ => "Unknown Library".to_string(),
    };

    Ok(LoanAcceptResult {
        lender_name,
        due_date: due.format("%Y-%m-%d").to_string(),
        book_isbn: book.isbn,
        book_title: book.title,
        book_cover_url: book.cover_url,
    })
}

/// Try to send a message to a peer via E2EE. Returns Ok(Some(response)) if E2EE succeeded,
/// Ok(None) if E2EE is not available for this peer (caller should fall back to plaintext).
///
/// ADR-012: All message types now support relay fallback. Request-response messages
/// (search_request, book_sync_request, library_*) attach reply_to fields so the
/// responder can deposit the answer in our mailbox.
async fn try_send_e2ee(
    state: &crate::infrastructure::AppState,
    peer: &peer::Model,
    message_type: &str,
    payload: serde_json::Value,
) -> Result<Option<Option<crate::crypto::envelope::ClearMessage>>, String> {
    // Check if peer supports E2EE
    if !peer.key_exchange_done {
        tracing::warn!(
            "E2EE: Skipping - peer {} key_exchange_done=false",
            peer.name
        );
        return Ok(None); // Plaintext fallback
    }

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            tracing::warn!("E2EE: Skipping - CryptoService not initialized");
            return Ok(None); // Identity not ready, fallback
        }
    };

    // Parse peer's X25519 public key
    let x25519_hex = match &peer.x25519_public_key {
        Some(hex) => hex,
        None => {
            tracing::warn!(
                "E2EE: Skipping - peer {} missing x25519_public_key",
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
            // Network error - peer unreachable. Try relay fallback.
            // ADR-012: All message types can now be relayed. Request-response messages
            // attach reply_to fields so responses come back via our mailbox.
            if let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
                (&peer.relay_url, &peer.mailbox_id, &peer.relay_write_token)
            {
                tracing::info!(
                    "E2EE: Direct failed ({}), trying relay for '{}' to peer {}",
                    net_err,
                    message_type,
                    peer.name
                );

                // For relay messages, attach reply_to fields from our relay config
                // so the responder can deposit the answer in our mailbox.
                let mut relay_message = message.clone();
                let mut correlation_id_for_await: Option<String> = None;

                // Only await relay responses for request-response types.
                // Fire-and-forget types (loan_confirmation, status_update, etc.)
                // are deposited in the peer's mailbox but we don't block waiting.
                const RELAY_AWAIT_RESPONSE: &[&str] = &[
                    "loan_request",
                    "book_sync_request",
                    "search_request",
                    "device_sync_request",
                    "library_manifest_request",
                    "library_page_request",
                    "library_search_request",
                    "request_status_query",
                ];
                let needs_response = RELAY_AWAIT_RESPONSE.contains(&message_type);

                if let Some(my_config) = crate::api::relay::get_my_relay_config(state.db()).await {
                    let correlation_id = uuid::Uuid::new_v4().to_string();
                    relay_message.correlation_id = Some(correlation_id.clone());
                    relay_message.reply_to_mailbox = Some(my_config.mailbox_uuid.clone());
                    relay_message.reply_to_write_token = Some(my_config.write_token.clone());
                    if needs_response {
                        correlation_id_for_await = Some(correlation_id);
                    }
                }

                let relay =
                    crate::services::relay_transport::RelayTransport::new(Some(crypto_service));

                // Try relay send, with automatic retry on 404 (expired mailbox)
                let relay_send_ok = match relay
                    .send(
                        relay_url,
                        mailbox_id,
                        write_token,
                        &peer_x25519,
                        &relay_message,
                    )
                    .await
                {
                    Ok(()) => true,
                    Err(crate::services::e2ee_transport::E2eeTransportError::PeerError(
                        404,
                        ref body,
                    )) => {
                        // Peer's mailbox expired/deleted on the hub.
                        // Try to refresh their relay credentials from /api/config.
                        tracing::warn!(
                            "E2EE Relay: Peer {} mailbox not found ({}), attempting credential refresh",
                            peer.name,
                            body
                        );
                        if let Some(refreshed) =
                            refresh_peer_relay_credentials(state.db(), peer).await
                        {
                            match relay
                                .send(
                                    &refreshed.0,
                                    &refreshed.1,
                                    &refreshed.2,
                                    &peer_x25519,
                                    &relay_message,
                                )
                                .await
                            {
                                Ok(()) => {
                                    tracing::info!(
                                        "E2EE Relay: Retry succeeded for peer {} with refreshed credentials",
                                        peer.name
                                    );
                                    true
                                }
                                Err(retry_err) => {
                                    if let Some(corr_id) = correlation_id_for_await {
                                        state.cancel_relay_request(&corr_id);
                                    }
                                    return Err(format!(
                                        "E2EE relay failed after credential refresh: {retry_err}"
                                    ));
                                }
                            }
                        } else {
                            if let Some(corr_id) = correlation_id_for_await {
                                state.cancel_relay_request(&corr_id);
                            }
                            return Err(format!(
                                "E2EE relay: peer {} mailbox expired, peer unreachable for credential refresh",
                                peer.name
                            ));
                        }
                    }
                    Err(relay_err) => {
                        if let Some(corr_id) = correlation_id_for_await {
                            state.cancel_relay_request(&corr_id);
                        }
                        tracing::warn!(
                            "E2EE Relay: Also failed for peer {}: {relay_err}",
                            peer.name
                        );
                        return Err(format!(
                            "E2EE send failed (direct: {net_err}, relay: {relay_err})"
                        ));
                    }
                };

                if relay_send_ok {
                    tracing::info!(
                        "E2EE Relay: Sent '{}' to peer {} via relay",
                        message_type,
                        peer.name
                    );

                    // Await the relay response with periodic polling instead
                    // of returning 202 and relying on Flutter adaptive polling.
                    // 65s covers one full remote poller cycle (60s + jitter).
                    if let Some(corr_id) = correlation_id_for_await {
                        let mut rx = state.register_relay_request(corr_id.clone());
                        let start = std::time::Instant::now();
                        let overall_timeout = std::time::Duration::from_secs(90);

                        // Trigger immediate poll (don't wait for 60s background cycle)
                        let _ = crate::services::relay_poller::poll_once(state).await;

                        loop {
                            tokio::select! {
                                result = &mut rx => {
                                    match result {
                                        Ok(payload) => {
                                            tracing::info!(
                                                "E2EE Relay: Got response for '{}' from peer {} ({}ms)",
                                                message_type,
                                                peer.name,
                                                start.elapsed().as_millis()
                                            );
                                            let response_msg = crate::crypto::envelope::ClearMessage {
                                                message_type: format!("{message_type}_response"),
                                                payload,
                                                timestamp: chrono::Utc::now().timestamp(),
                                                message_id: uuid::Uuid::new_v4().to_string(),
                                                correlation_id: Some(corr_id),
                                                reply_to_mailbox: None,
                                                reply_to_write_token: None,
                                            };
                                            return Ok(Some(Some(response_msg)));
                                        }
                                        Err(_) => {
                                            return Ok(Some(None));
                                        }
                                    }
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                                    if start.elapsed() >= overall_timeout {
                                        tracing::info!(
                                            "E2EE Relay: Timeout waiting for '{}' response from peer {} ({}s)",
                                            message_type,
                                            peer.name,
                                            start.elapsed().as_secs()
                                        );
                                        state.cancel_relay_request(&corr_id);
                                        return Ok(Some(None));
                                    }
                                    let _ = crate::services::relay_poller::poll_once(state).await;
                                }
                            }
                        }
                    }

                    return Ok(Some(None));
                }
            }

            Err(format!("E2EE send failed: network error: {net_err}"))
        }
        Err(e) => Err(format!("E2EE send failed: {e}")),
    }
}

/// Attempt to refresh a peer's relay credentials.
///
/// Strategy:
///   1. LAN peers: fetch `/api/config` directly (fast, no hub dependency).
///   2. Relay-only peers: query the hub directory for updated credentials.
///      The hub only returns relay fields to authenticated requesters, and
///      the caller verifies the x25519 key matches before trusting them.
///
/// Returns `Some((relay_url, mailbox_id, write_token))` on success.
/// Updates the peer record in the database.
async fn refresh_peer_relay_credentials(
    db: &DatabaseConnection,
    peer_model: &peer::Model,
) -> Option<(String, String, String)> {
    let (relay_url, mailbox_id, write_token) = if peer_model.url.starts_with("relay://") {
        // Relay-only: query hub directory for updated credentials
        refresh_via_hub(db, peer_model).await?
    } else {
        // LAN peer: direct HTTP fetch
        refresh_via_lan(peer_model).await?
    };

    if relay_url.is_empty() || mailbox_id.is_empty() || write_token.is_empty() {
        return None;
    }

    // Update peer record with fresh relay credentials
    if let Ok(Some(existing)) = peer::Entity::find_by_id(peer_model.id).one(db).await {
        let mut active: peer::ActiveModel = existing.into();
        active.relay_url = Set(Some(relay_url.clone()));
        active.mailbox_id = Set(Some(mailbox_id.clone()));
        active.relay_write_token = Set(Some(write_token.clone()));
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active.update(db).await;
        tracing::info!(
            "Relay: Refreshed credentials for peer '{}' (mailbox: {})",
            peer_model.name,
            mailbox_id
        );
    }

    Some((relay_url, mailbox_id, write_token))
}

/// Refresh relay credentials via direct HTTP to the peer's LAN URL.
async fn refresh_via_lan(peer_model: &peer::Model) -> Option<(String, String, String)> {
    let client = get_safe_client();
    let config_url = format!("{}/api/config", peer_model.url.trim_end_matches('/'));

    let response = client.get(&config_url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let config: crate::api::setup::ConfigResponse = response.json().await.ok()?;
    Some((
        config.relay_url?,
        config.mailbox_id?,
        config.relay_write_token?,
    ))
}

/// Refresh relay credentials via the hub directory (for relay-only peers).
///
/// Queries the hub for the peer's profile using their library_uuid (node_id).
/// Verifies the x25519 key matches before trusting the returned credentials.
async fn refresh_via_hub(
    db: &DatabaseConnection,
    peer_model: &peer::Model,
) -> Option<(String, String, String)> {
    let hub_url =
        crate::services::hub_directory_service::HubDirectoryService::hub_base_url().ok()?;
    let peer_node_id = peer_model.library_uuid.as_deref()?;

    // Authenticate with our own write_token
    let our_config = crate::services::hub_directory_service::HubDirectoryService::get_config(db)
        .await
        .ok()
        .flatten()?;

    let client = get_safe_client();
    let url = format!("{hub_url}/api/directory/profile/{peer_node_id}");
    let response = client
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", our_config.write_token),
        )
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        tracing::debug!(
            "Relay: Hub profile lookup failed for peer '{}' (status {})",
            peer_model.name,
            response.status()
        );
        return None;
    }

    let profile: crate::services::hub_directory_service::HubProfile = response.json().await.ok()?;

    // Verify x25519 key matches what we have locally to prevent
    // an attacker from redirecting messages to their own mailbox.
    if let Some(ref local_key) = peer_model.x25519_public_key
        && profile.x25519_public_key.as_deref() != Some(local_key.as_str())
    {
        tracing::warn!(
            "Relay: Hub profile x25519 key mismatch for peer '{}', rejecting credentials",
            peer_model.name
        );
        return None;
    }

    let relay_url = profile.relay_url?;
    let mailbox_id = profile.relay_mailbox_id?;
    let write_token = profile.relay_write_token?;

    tracing::info!(
        "Relay: Refreshed credentials for relay-only peer '{}' via hub (mailbox: {})",
        peer_model.name,
        mailbox_id
    );

    Some((relay_url, mailbox_id, write_token))
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

/// DELETE /api/peers/relay/config - Remove relay config (disconnect from hub).
pub async fn delete_relay_config_endpoint(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    use sea_orm::ConnectionTrait;
    match db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM my_relay_config".to_owned(),
        ))
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Relay config removed" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to remove relay config: {e}") })),
        )
            .into_response(),
    }
}

// ── Relay library sync endpoints (ADR-012) ──────────────────────────

/// Send a library sync request to a peer via E2EE (relay or direct).
/// Returns the response payload if available, or starts async relay flow.
///
/// POST /api/peers/relay/library_request
/// Body: { "peer_id": int, "request_type": "manifest"|"page"|"search", ... }
#[derive(Deserialize)]
pub struct RelayLibraryRequest {
    pub peer_id: i32,
    pub request_type: String,
    #[serde(default)]
    pub cursor: Option<i64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub query: Option<String>,
}

pub async fn relay_library_request(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<RelayLibraryRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find the peer
    let the_peer = match peer::Entity::find_by_id(req.peer_id).one(db).await {
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

    // 2. Build the E2EE message type and payload
    let (message_type, payload) = match req.request_type.as_str() {
        "manifest" => ("library_manifest_request", json!({})),
        "page" => (
            "library_page_request",
            json!({
                "cursor": req.cursor,
                "limit": req.limit.unwrap_or(50),
            }),
        ),
        "search" => (
            "library_search_request",
            json!({
                "query": req.query.unwrap_or_default(),
                "limit": req.limit.unwrap_or(20),
            }),
        ),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid request_type. Use: manifest, page, search" })),
            )
                .into_response();
        }
    };

    // 3. Send via E2EE (direct or relay with reply-to)
    match try_send_e2ee(&state, &the_peer, message_type, payload).await {
        Ok(Some(Some(response))) => {
            // Direct response (LAN path)
            (StatusCode::OK, Json(response.payload)).into_response()
        }
        Ok(Some(None)) => {
            // Sent via relay (no immediate response)
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "relay_pending",
                    "message": "Request sent via relay. Use poll_now to check for response.",
                })),
            )
                .into_response()
        }
        Ok(None) => {
            // E2EE not available - no plaintext fallback for library sync
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "E2EE not available for this peer" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Wait for a pending relay response by correlation_id.
///
/// POST /api/peers/relay/await_response
/// Body: { "correlation_id": "uuid", "timeout_ms": 5000 }
#[derive(Deserialize)]
pub struct AwaitRelayResponse {
    pub correlation_id: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

pub async fn await_relay_response(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<AwaitRelayResponse>,
) -> impl IntoResponse {
    let timeout = std::time::Duration::from_millis(req.timeout_ms.min(30_000));

    // Register a new listener (or check if one already exists)
    let rx = state.register_relay_request(req.correlation_id.clone());

    // Wait for the response with timeout
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(payload)) => (StatusCode::OK, Json(payload)).into_response(),
        Ok(Err(_)) => {
            // Sender dropped (cancelled)
            (
                StatusCode::GONE,
                Json(json!({ "error": "Request was cancelled" })),
            )
                .into_response()
        }
        Err(_) => {
            // Timeout - clean up
            state.cancel_relay_request(&req.correlation_id);
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(json!({ "status": "timeout", "message": "No response yet" })),
            )
                .into_response()
        }
    }
}

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
                        tracing::info!(
                            "Peer {} library_uuid changed, clearing cached books",
                            peer_id
                        );
                        let _ = peer_book::Entity::delete_many()
                            .filter(peer_book::Column::PeerId.eq(peer_id))
                            .exec(&db)
                            .await;
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

/// Upsert peer books cache: preserves `first_seen_at` for existing entries,
/// sets it to now for new entries (or NULL on initial sync to suppress the
/// "new" badge when all books are discovered at once).
/// Removes books no longer in the fresh list.
/// Returns the number of books in the fresh list.
async fn upsert_peer_books_cache(
    db: &DatabaseConnection,
    peer_id: i32,
    node_id: Option<&str>,
    books: Vec<crate::models::Book>,
) -> usize {
    let now = chrono::Utc::now().to_rfc3339();
    let count = books.len();

    // 1. Load existing cached books for this peer
    let existing = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(db)
        .await
        .unwrap_or_default();

    let existing_map: std::collections::HashMap<i32, peer_book::Model> = existing
        .into_iter()
        .map(|e| (e.remote_book_id, e))
        .collect();

    let mut fresh_ids = std::collections::HashSet::new();

    // On initial discovery (no existing books for this peer), all books arrive
    // at once so marking them all as "new" is meaningless noise. Set first_seen_at
    // to NULL so the badge only appears for books added after the first sync.
    let is_initial_sync = existing_map.is_empty();

    // 2. Upsert each book
    for book in books {
        let remote_id = book.id.unwrap_or(0);
        fresh_ids.insert(remote_id);

        if let Some(existing_entry) = existing_map.get(&remote_id) {
            // UPDATE: preserve first_seen_at and notified_at, refresh other fields
            let mut active: peer_book::ActiveModel = existing_entry.clone().into();
            active.title = Set(book.title);
            active.isbn = Set(book.isbn);
            active.author = Set(book.author);
            active.cover_url = Set(book.cover_url);
            active.summary = Set(book.summary);
            active.synced_at = Set(now.clone());
            if let Some(nid) = node_id {
                active.node_id = Set(Some(nid.to_string()));
            }
            // first_seen_at and notified_at stay unchanged
            let _ = active.update(db).await;
        } else {
            // INSERT: new book (notified_at = NULL - not yet notified)
            let cache = peer_book::ActiveModel {
                peer_id: Set(peer_id),
                remote_book_id: Set(remote_id),
                title: Set(book.title),
                isbn: Set(book.isbn),
                author: Set(book.author),
                cover_url: Set(book.cover_url),
                summary: Set(book.summary),
                synced_at: Set(now.clone()),
                node_id: Set(node_id.map(|s| s.to_string())),
                first_seen_at: Set(if is_initial_sync {
                    None
                } else {
                    Some(now.clone())
                }),
                notified_at: Set(None),
                ..Default::default()
            };
            let _ = peer_book::Entity::insert(cache).exec(db).await;
        }
    }

    // 3. Delete books no longer in the fresh list
    for (remote_id, entry) in &existing_map {
        if !fresh_ids.contains(remote_id) {
            let _ = peer_book::Entity::delete_by_id(entry.id).exec(db).await;
        }
    }

    // 4. Check un-notified books against wishlist + emit "new_books" notification.
    // Uses notified_at IS NULL instead of tracking inserts in memory, so that
    // notification dedup survives notification pruning (TTL/cap).
    let unnotified = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .filter(peer_book::Column::NotifiedAt.is_null())
        .all(db)
        .await
        .unwrap_or_default();

    if !unnotified.is_empty() {
        let new_isbns: Vec<(String, String)> = unnotified
            .iter()
            .filter_map(|pb| {
                pb.isbn
                    .as_ref()
                    .map(|isbn| (isbn.clone(), pb.title.clone()))
            })
            .collect();

        let peer_name = peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default();
        let ref_id = peer_id.to_string();

        // Wishlist matches
        if !new_isbns.is_empty() {
            crate::services::notification_service::check_wishlist_matches(
                db, &new_isbns, &peer_name, "peer", &ref_id,
            )
            .await;
        }

        // Mark all un-notified books as notified so they won't trigger again
        for pb in unnotified {
            let mut active: peer_book::ActiveModel = pb.into();
            active.notified_at = Set(Some(now.clone()));
            let _ = active.update(db).await;
        }
    }

    count
}

/// Internal sync function for background sync after connect
async fn sync_peer_internal(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
) -> Result<usize, String> {
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

    // Distinguish "peer explicitly disallows caching" from "peer unreachable"
    let peer_reachable = peer_config.is_some();
    let allows_caching = peer_config
        .as_ref()
        .map(|c| c.allow_library_caching)
        .unwrap_or(true); // assume caching OK when unreachable - preserve cache
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));

    // Extract updated name and avatar from peer config (single DB read)
    let peer_library_name = peer_config.as_ref().map(|c| c.library_name.clone());
    let (updated_name, updated_avatar) = if let Some(config) = &peer_config {
        if let Ok(Some(p)) = peer::Entity::find_by_id(peer_id).one(db).await {
            let name = if p.name != config.library_name {
                Some(config.library_name.clone())
            } else {
                None
            };
            let avatar_json = config
                .avatar_config
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let avatar = if avatar_json != p.avatar_config {
                avatar_json
            } else {
                None
            };
            (name, avatar)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Resolve peer display name for memory score upsert
    let display_name = peer_library_name.as_deref().unwrap_or(peer_url);

    if peer_reachable && !allows_caching {
        tracing::info!(
            "Peer {} explicitly disallows library caching, clearing cache",
            peer_url
        );
        // Peer is reachable and explicitly disallows caching - clear cache
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .exec(db)
            .await;
        // Still sync gamification stats if available
        sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;
        // Still sync memory game scores
        crate::modules::memory_game::handlers::sync_peer_memory_scores(
            db,
            peer_id,
            peer_url,
            display_name,
            &client,
            peer_has_memory_game,
        )
        .await;
        // Still sync sliding puzzle scores
        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
            db,
            peer_id,
            peer_url,
            display_name,
            &client,
            peer_has_sliding_puzzle,
        )
        .await;
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
            if let Some(ref avatar) = updated_avatar {
                active_peer.avatar_config = Set(Some(avatar.clone()));
            }
            active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active_peer.update(db).await;
        }
        return Ok(0); // Return 0 books cached
    }

    // Fetch remote books (owned only - exclude books the peer borrowed from others)
    let url = format!("{}/api/books?owned_only=true", peer_url);

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

    // Upsert books cache (preserves first_seen_at for existing entries)
    let count = upsert_peer_books_cache(db, peer_id, None, data.books).await;

    // Sync gamification stats if both sides have the module enabled
    sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;

    // Sync memory game scores
    crate::modules::memory_game::handlers::sync_peer_memory_scores(
        db,
        peer_id,
        peer_url,
        display_name,
        &client,
        peer_has_memory_game,
    )
    .await;

    // Sync sliding puzzle scores
    crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
        db,
        peer_id,
        peer_url,
        display_name,
        &client,
        peer_has_sliding_puzzle,
    )
    .await;

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
        if let Some(ref avatar) = updated_avatar {
            active_peer.avatar_config = Set(Some(avatar.clone()));
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
                Ok(_) => {
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

pub async fn list_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // Legacy hub peer sync removed: peers are managed locally via invite
    // links, QR codes, and mDNS discovery. The old GET /api/peers hub
    // endpoint was causing SQLite lock contention and timeouts on every
    // list_peers call, making peers appear to vanish from the UI.

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
                "display_name": p.display_name,
                "url": p.url,
                "public_key": p.public_key,
                "library_uuid": p.library_uuid,
                "latitude": p.latitude,
                "longitude": p.longitude,
                "auto_approve": p.auto_approve,
                "connection_status": p.connection_status,
                "status": status,
                "relay_url": p.relay_url,
                "mailbox_id": p.mailbox_id,
                "relay_write_token": p.relay_write_token,
                "last_seen": p.last_seen,
                "avatar_config": p.avatar_config,
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
            tracing::info!("Peer {} accepted, auto_approve={}", peer_id, auto_approve);

            // Emit connection_accepted notification
            if auto_approve {
                crate::services::notification_service::emit(
                    &db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::ConnectionAccepted,
                        title: updated.name.clone(),
                        body: None,
                        ref_type: Some("peer".to_string()),
                        ref_id: Some(peer_id.to_string()),
                    },
                )
                .await;
            }

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
    /// Optional library_uuid to backfill when discovered via mDNS.
    /// Validated as a proper UUID to prevent injection.
    pub library_uuid: Option<String>,
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

    // Security: Only update URL for pending peers, unless upgrading from relay
    // to LAN or fixing a port mismatch (mDNS discovered the correct address).
    // This endpoint is localhost-only, so the caller is always the local app.
    if peer.auto_approve && !peer.url.starts_with("relay://") {
        // Allow port updates for same-host LAN URLs (hot restart changes port)
        let same_host = match (url::Url::parse(&peer.url), url::Url::parse(&payload.url)) {
            (Ok(old), Ok(new_url)) => old.host() == new_url.host(),
            _ => false,
        };
        if !same_host {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Cannot update URL for connected peers" })),
            )
                .into_response();
        }
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

    // Backfill library_uuid if provided and valid UUID format
    if let Some(ref uuid_str) = payload.library_uuid {
        if uuid::Uuid::parse_str(uuid_str).is_ok() {
            active_model.library_uuid = Set(Some(uuid_str.clone()));
            tracing::info!(
                "Backfilling library_uuid for peer {}: {}",
                peer_id,
                uuid_str
            );
        } else {
            tracing::warn!(
                "Ignoring invalid library_uuid for peer {}: {}",
                peer_id,
                uuid_str
            );
        }
    }

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
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Load peer before deletion so we can notify the remote side
    let peer_model = match peer::Entity::find_by_id(peer_id).one(db).await {
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

    // 2. Notify remote peer (fire-and-forget, never blocks local deletion)
    let state_clone = state.clone();
    let peer_clone = peer_model.clone();
    tokio::spawn(async move {
        notify_peer_of_disconnect(&state_clone, &peer_clone).await;
    });

    // 3. Delete locally
    match peer::Entity::delete_by_id(peer_id).exec(db).await {
        Ok(_) => {
            tracing::info!("🗑️ Peer {} ({}) deleted", peer_id, peer_model.name);
            (StatusCode::OK, Json(json!({ "message": "Peer deleted" }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
    }
}

/// Notify a remote peer that we are disconnecting.
///
/// Tries E2EE first (encrypted, with relay fallback for offline peers),
/// then falls back to a plaintext HTTP POST for peers without E2EE keys.
/// Errors are logged but never propagated - disconnection is always local-first.
async fn notify_peer_of_disconnect(
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
    use sea_orm::sea_query::Expr;

    let books = book::Entity::find()
        .filter(book::Column::Private.eq(false))
        .filter(
            Condition::any()
                .add(book::Column::Title.contains(&payload.query))
                .add(
                    Expr::col(book::Column::Id)
                        .in_subquery(crate::models::Book::author_search_subquery(&payload.query)),
                ),
        )
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let book_dtos = crate::models::Book::populate_authors(&db, books).await;
    (StatusCode::OK, Json(book_dtos)).into_response()
}

#[derive(Deserialize)]
pub struct ProxySearchRequest {
    peer_id: Option<i32>,
    peer_url: Option<String>,
    query: String,
    page: Option<u64>,
    limit: Option<u64>,
}

/// Plaintext HTTP proxy: fetch books from a peer URL directly.
/// When `page`/`limit` are provided, returns `{ "books": [...], "total": N, "has_more": bool }`.
/// Without pagination params, returns a flat `Vec<Book>` array (legacy).
async fn plaintext_proxy_search(
    peer_url: &str,
    query: &str,
    page: Option<u64>,
    limit: Option<u64>,
) -> axum::response::Response {
    let client = get_safe_client();
    let res = if query.is_empty() {
        let mut url = format!("{}/api/books?owned_only=true", peer_url);
        if let Some(p) = page {
            let l = limit.unwrap_or(20).min(50);
            url.push_str(&format!("&page={}&limit={}", p, l));
        }
        client.get(&url).send().await
    } else {
        let url = format!("{}/api/peers/search", peer_url);
        client
            .post(&url)
            .json(&json!({ "query": query }))
            .send()
            .await
    };

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // /api/books returns {"books": [...], "total": N}
                // /api/peers/search returns [...]
                let body: serde_json::Value = response.json().await.unwrap_or(json!([]));

                if page.is_some() && query.is_empty() {
                    // Paginated: return envelope with has_more
                    let books_val = body.get("books").cloned().unwrap_or(json!([]));
                    let total = body.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                    let p = page.unwrap_or(0);
                    let l = limit.unwrap_or(20).min(50);
                    let has_more = ((p + 1) * l) < total;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "books": books_val,
                            "total": total,
                            "has_more": has_more,
                        })),
                    )
                        .into_response()
                } else {
                    // Legacy: return flat array
                    let books: Vec<crate::models::Book> = if let Some(arr) = body.get("books") {
                        serde_json::from_value(arr.clone()).unwrap_or_default()
                    } else {
                        serde_json::from_value(body).unwrap_or_default()
                    };
                    (StatusCode::OK, Json(books)).into_response()
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned an error" })),
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

        // Paginated library browse via E2EE (empty query + page param)
        if payload.query.is_empty() && payload.page.is_some() {
            let page = payload.page.unwrap_or(0);
            let limit = payload.limit.unwrap_or(20).min(50);
            match try_send_e2ee(
                &state,
                &peer,
                "library_browse_request",
                json!({ "page": page, "limit": limit }),
            )
            .await
            {
                Ok(Some(Some(response_msg))) => {
                    return (StatusCode::OK, Json(response_msg.payload)).into_response();
                }
                Ok(Some(None)) | Ok(None) | Err(_) => {
                    // E2EE browse not supported or failed — fall back to plaintext paginated
                    return plaintext_proxy_search(
                        &peer.url,
                        &payload.query,
                        payload.page,
                        payload.limit,
                    )
                    .await;
                }
            }
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
        return plaintext_proxy_search(&peer.url, &payload.query, payload.page, payload.limit)
            .await;
    }

    // Peer not in DB but URL provided (e.g. unsaved mDNS peer): direct plaintext fetch
    if let Some(ref url) = payload.peer_url {
        if let Err(e) = validate_url(url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }
        return plaintext_proxy_search(url, &payload.query, payload.page, payload.limit).await;
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
    let peer_has_memory_game = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));
    let peer_display_name = peer_config
        .as_ref()
        .map(|c| c.library_name.clone())
        .unwrap_or_else(|| peer.name.clone());

    // Update library_uuid: backfill if missing, or detect changes (peer reset).
    // Validates UUID format to prevent a malicious peer from injecting arbitrary strings.
    if let Some(remote_uuid) = peer_config.as_ref().and_then(|c| c.library_uuid.clone()) {
        if uuid::Uuid::parse_str(&remote_uuid).is_ok() {
            let uuid_changed = peer
                .library_uuid
                .as_ref()
                .is_some_and(|old| old != &remote_uuid);
            let uuid_missing = peer.library_uuid.is_none();

            if uuid_changed || uuid_missing {
                let mut active: peer::ActiveModel = peer.clone().into();
                active.library_uuid = Set(Some(remote_uuid.clone()));
                if let Err(e) = active.update(&db).await {
                    tracing::warn!("Failed to update library_uuid for peer {}: {}", peer_id, e);
                } else if uuid_changed {
                    // Peer was reset/reinstalled - clear stale cached books
                    tracing::info!(
                        "Peer {} library_uuid changed during sync, clearing cached books",
                        peer_id
                    );
                    let _ = peer_book::Entity::delete_many()
                        .filter(peer_book::Column::PeerId.eq(peer_id))
                        .exec(&db)
                        .await;
                } else {
                    tracing::info!("Backfilled library_uuid for peer {}", peer_id);
                }
            }
        } else {
            tracing::warn!("Peer {} sent invalid library_uuid, ignoring", peer_id);
        }
    }

    let url = format!("{}/api/books?owned_only=true", peer.url);

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
                        // Upsert books cache (preserves first_seen_at)
                        let count = upsert_peer_books_cache(&db, peer.id, None, data.books).await;

                        // Sync gamification stats
                        sync_peer_gamification_stats(
                            &db,
                            peer.id,
                            &peer.url,
                            &client,
                            shares_gamification,
                        )
                        .await;

                        // Sync memory game scores
                        crate::modules::memory_game::handlers::sync_peer_memory_scores(
                            &db,
                            peer.id,
                            &peer.url,
                            &peer_display_name,
                            &client,
                            peer_has_memory_game,
                        )
                        .await;

                        // Sync sliding puzzle scores
                        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
                            &db,
                            peer.id,
                            &peer.url,
                            &peer_display_name,
                            &client,
                            peer_has_sliding_puzzle,
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
    let mut peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };

    // 4b. If config fetch failed, the peer may have restarted on a different port.
    // Try scanning ports 8000-8010 on the same host.
    let effective_url = if peer_config.is_none() {
        match crate::utils::peer_discovery::try_discover_peer_port(&peer.url, &client).await {
            Some(new_url) => {
                // Retry config fetch with discovered URL
                let retry_url = format!("{}/api/config", new_url);
                peer_config = match client.get(&retry_url).send().await {
                    Ok(res) if res.status().is_success() => {
                        res.json::<crate::api::setup::ConfigResponse>().await.ok()
                    }
                    _ => None,
                };
                new_url
            }
            None => peer.url.clone(),
        }
    } else {
        peer.url.clone()
    };

    // Distinguish "peer explicitly disallows caching" from "peer unreachable"
    // When peer_config is None (unreachable on 5G), preserve cache and try E2EE/relay
    let peer_reachable = peer_config.is_some();
    let allows_caching = peer_config
        .as_ref()
        .map(|c| c.allow_library_caching)
        .unwrap_or(true); // assume caching OK when unreachable - preserve cache
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game_url = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle_url = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));
    let peer_display_name_url = peer_config
        .as_ref()
        .map(|c| c.library_name.clone())
        .unwrap_or_else(|| peer.name.clone());

    // Extract updated name from peer config (if changed)
    let updated_name = peer_config
        .as_ref()
        .filter(|c| c.library_name != peer.name)
        .map(|c| c.library_name.clone());

    // Extract updated avatar config from peer config (if changed)
    let updated_avatar = peer_config.as_ref().and_then(|c| {
        let new_json = c
            .avatar_config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        tracing::info!(
            "Sync avatar check for peer {}: remote={:?}, stored={:?}",
            peer.name,
            new_json.as_deref().map(|s| &s[..s.len().min(50)]),
            peer.avatar_config.as_deref().map(|s| &s[..s.len().min(50)]),
        );
        if new_json != peer.avatar_config {
            new_json
        } else {
            None
        }
    });

    // Refresh relay credentials from peer config if they changed
    if let Some(ref config) = peer_config {
        let new_relay = (
            config.relay_url.as_deref(),
            config.mailbox_id.as_deref(),
            config.relay_write_token.as_deref(),
        );
        let old_relay = (
            peer.relay_url.as_deref(),
            peer.mailbox_id.as_deref(),
            peer.relay_write_token.as_deref(),
        );
        if new_relay != old_relay
            && let (Some(r_url), Some(m_id), Some(w_tok)) = new_relay
            && !r_url.is_empty()
            && !m_id.is_empty()
            && !w_tok.is_empty()
            && let Ok(Some(existing)) = peer::Entity::find_by_id(peer.id).one(&db).await
        {
            let mut active: peer::ActiveModel = existing.into();
            active.relay_url = Set(Some(r_url.to_string()));
            active.mailbox_id = Set(Some(m_id.to_string()));
            active.relay_write_token = Set(Some(w_tok.to_string()));
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(&db).await;
            tracing::info!(
                "Sync: Updated relay credentials for peer {} (mailbox: {})",
                peer.name,
                m_id
            );
        }
    }

    if peer_reachable && !allows_caching {
        // Peer is reachable and explicitly disallows caching - clear cache
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer.id))
            .exec(&db)
            .await;
        // Peer doesn't allow caching - still sync gamification stats
        sync_peer_gamification_stats(&db, peer.id, &effective_url, &client, shares_gamification)
            .await;
        // Still sync memory game scores
        crate::modules::memory_game::handlers::sync_peer_memory_scores(
            &db,
            peer.id,
            &effective_url,
            &peer_display_name_url,
            &client,
            peer_has_memory_game_url,
        )
        .await;
        // Still sync sliding puzzle scores
        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
            &db,
            peer.id,
            &effective_url,
            &peer_display_name_url,
            &client,
            peer_has_sliding_puzzle_url,
        )
        .await;

        let peer_id = peer.id;
        let url_changed = effective_url != peer.url;
        let mut active_peer: peer::ActiveModel = peer.into();
        if url_changed {
            active_peer.url = Set(effective_url);
            tracing::info!("Port discovery: persisted new URL for peer {}", peer_id);
        }
        if let Some(ref new_name) = updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        if let Some(ref avatar) = updated_avatar {
            active_peer.avatar_config = Set(Some(avatar.clone()));
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
                let url = format!("{}/api/books?owned_only=true", effective_url);
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

    // 5. Upsert books cache (preserves first_seen_at)
    let count = upsert_peer_books_cache(&db, peer.id, None, books).await;

    // 6. Sync gamification stats
    sync_peer_gamification_stats(&db, peer.id, &effective_url, &client, shares_gamification).await;

    // 6b. Sync memory game scores
    crate::modules::memory_game::handlers::sync_peer_memory_scores(
        &db,
        peer.id,
        &effective_url,
        &peer_display_name_url,
        &client,
        peer_has_memory_game_url,
    )
    .await;

    // 6c. Sync sliding puzzle scores
    crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
        &db,
        peer.id,
        &effective_url,
        &peer_display_name_url,
        &client,
        peer_has_sliding_puzzle_url,
    )
    .await;

    // 7. Update peer's last_seen (and name/URL if changed)
    let peer_id = peer.id;
    let url_changed = effective_url != peer.url;
    let mut active_peer: peer::ActiveModel = peer.into();
    if url_changed {
        active_peer.url = Set(effective_url);
        tracing::info!("Port discovery: persisted new URL for peer {}", peer_id);
    }
    if let Some(ref new_name) = updated_name {
        active_peer.name = Set(new_name.clone());
        tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
    }
    if let Some(ref avatar) = updated_avatar {
        active_peer.avatar_config = Set(Some(avatar.clone()));
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

/// Borrower-side: process an auto-approve acceptance from the lender's synchronous response.
///
/// Updates the outgoing request to "accepted" and creates a borrowed copy in the local library.
/// Called from both E2EE and plaintext paths when the lender auto-accepts.
async fn process_borrower_acceptance(
    db: &DatabaseConnection,
    outgoing_id: &str,
    payload: &serde_json::Value,
    lender_request_id: Option<&str>,
) {
    use crate::models::{book, copy, p2p_outgoing_request};

    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let isbn = payload.get("isbn").and_then(|v| v.as_str());
    let cover_url = payload
        .get("cover_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let lender_name = payload
        .get("lender_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let due_date = payload
        .get("due_date")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");

    if title.is_empty() {
        tracing::warn!("process_borrower_acceptance: empty title, skipping");
        return;
    }

    // 1. Update outgoing request to "accepted"
    if let Ok(Some(outgoing)) = p2p_outgoing_request::Entity::find_by_id(outgoing_id)
        .one(db)
        .await
    {
        let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
        active.status = Set("accepted".to_string());
        if let Some(lr_id) = lender_request_id {
            active.lender_request_id = Set(Some(lr_id.to_string()));
        }
        active.updated_at = Set(Utc::now().to_rfc3339());
        let _ = active.update(db).await;
    }

    // 2. Find or create book
    let existing_book = if let Some(isbn_val) = isbn
        && !isbn_val.is_empty()
    {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn_val))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(title))
            .one(db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => b.id,
        None => {
            let now = Utc::now().to_rfc3339();
            let new_book = book::ActiveModel {
                title: Set(title.to_string()),
                isbn: Set(isbn.map(|s| s.to_string())),
                cover_url: Set(cover_url.clone()),
                owned: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            match new_book.insert(db).await {
                Ok(b) => b.id,
                Err(e) => {
                    tracing::error!("process_borrower_acceptance: failed to create book: {e}");
                    return;
                }
            }
        }
    };

    // 3. Idempotency: skip if a borrowed temporary copy already exists
    let existing_borrowed = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("borrowed"))
        .filter(copy::Column::IsTemporary.eq(true))
        .one(db)
        .await
        .ok()
        .flatten();

    if existing_borrowed.is_some() {
        tracing::info!(
            "process_borrower_acceptance: borrowed copy already exists for book_id={}",
            book_id
        );
        return;
    }

    // 4. Create borrowed copy
    let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("process_borrower_acceptance: failed to resolve library: {e}");
            return;
        }
    };
    let now = Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(lib_id),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {lender_name} jusqu'au {due_date}"
        ))),
        acquisition_date: Set(Some(now.clone())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(db).await {
        Ok(c) => {
            tracing::info!(
                "process_borrower_acceptance: created borrowed copy id={} for book_id={}",
                c.id,
                book_id
            );
            // Notify the borrower that the loan was accepted
            crate::services::notification_service::emit(
                db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowAccepted,
                    title: title.to_string(),
                    body: Some(lender_name.to_string()),
                    ref_type: Some("peer".to_string()),
                    ref_id: None,
                },
            )
            .await;
        }
        Err(e) => {
            tracing::error!("process_borrower_acceptance: failed to create copy: {e}");
        }
    }
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
    // Note: validate_url is deferred to the plaintext fallback path below.
    // Relay-only peers have a relay:// URL that is valid for E2EE but not for
    // direct HTTP, so SSRF validation must not block the E2EE path.

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
        "from_peer_url": state.our_public_url(),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(response)) => {
            // Check lender's synchronous response for auto-reject or auto-accept
            if let Some(ref clear_msg) = response {
                let status = clear_msg
                    .payload
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");

                if status == "rejected" {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (E2EE)",
                        outgoing_id
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": "no_available_copy" })),
                    )
                        .into_response();
                }

                if status == "accepted" {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (E2EE)",
                        outgoing_id
                    );
                    // Process acceptance: update outgoing request + create borrowed copy
                    let lender_request_id = clear_msg
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    process_borrower_acceptance(
                        db,
                        &outgoing_id,
                        &clear_msg.payload,
                        lender_request_id.as_deref(),
                    )
                    .await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
            }
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)", "status": "pending" })),
            )
                .into_response();
        }
        Ok(None) => {
            // Peer doesn't support E2EE, fall through to plaintext
        }
        Err(e) => {
            // E2EE transport error - both direct and relay failed.
            // Do NOT fall back to plaintext to avoid duplicate requests.
            tracing::warn!("E2EE send failed (no plaintext fallback): {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to deliver request to peer" })),
            )
                .into_response();
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Cannot reach peer: {}", e) })),
        )
            .into_response();
    }
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            let resp_status = response.status();
            let body = response.text().await.unwrap_or_default();

            if resp_status.is_success() {
                // Parse response body to check for auto-acceptance
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("accepted")
                {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (plaintext)",
                        outgoing_id
                    );
                    let lender_request_id = parsed.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(db, &outgoing_id, &parsed, lender_request_id).await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Request sent", "status": "pending" })),
                )
                    .into_response()
            } else {
                // Parse lender response to check for auto-rejection
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("rejected")
                {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    let reason = parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (plaintext): {}",
                        outgoing_id,
                        reason
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": reason })),
                    )
                        .into_response();
                }
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

    // 1. Find peer by URL (may be None for unsaved mDNS peers)
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(db)
        .await
    {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // Unsaved mDNS peer: skip outgoing request tracking, send plaintext directly
    if peer.is_none() {
        if let Err(e) = validate_url(&docker_url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }

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

        let request_payload = json!({
            "from_peer_url": state.our_public_url(),
            "from_peer_name": my_config.name,
            "book_isbn": payload.book_isbn,
            "book_title": payload.book_title,
            "requester_request_id": uuid::Uuid::new_v4().to_string()
        });

        let client = get_safe_client();
        let url = format!("{}/api/peers/request", docker_url);
        return match client.post(&url).json(&request_payload).send().await {
            Ok(response) if response.status().is_success() => {
                (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response()
            }
            Ok(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Peer rejected request" })),
            )
                .into_response(),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to contact peer" })),
            )
                .into_response(),
        };
    }

    let peer = peer.unwrap();

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
    // Note: validate_url is deferred to the plaintext fallback path below.
    // Relay-only peers have a relay:// URL that is valid for E2EE but not for
    // direct HTTP, so SSRF validation must not block the E2EE path.

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
        "from_peer_url": state.our_public_url(),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    // Try E2EE path first
    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(response)) => {
            // Check lender's synchronous response for auto-reject or auto-accept
            if let Some(ref clear_msg) = response {
                let status = clear_msg
                    .payload
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");
                if status == "rejected" {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (E2EE)",
                        outgoing_id
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": "no_available_copy" })),
                    )
                        .into_response();
                }

                if status == "accepted" {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (E2EE)",
                        outgoing_id
                    );
                    let lender_request_id = clear_msg
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    process_borrower_acceptance(
                        db,
                        &outgoing_id,
                        &clear_msg.payload,
                        lender_request_id.as_deref(),
                    )
                    .await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
            }
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)", "status": "pending" })),
            )
                .into_response();
        }
        Ok(None) => {
            // E2EE not available for this peer — fall back to plaintext.
        }
        Err(e) => {
            // E2EE transport error - both direct and relay failed.
            // Fall through to plaintext: if E2EE could not deliver at all
            // (peer unreachable or decryption failed on their side),
            // there is no duplicate risk.
            tracing::warn!("E2EE loan_request error, falling back to plaintext: {e}");
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    // SSRF validation: only needed here for direct HTTP to peer URL.
    // Relay-only peers (relay://) never reach this point because E2EE handles them.
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Cannot reach peer: {}", e) })),
        )
            .into_response();
    }
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            let resp_status = response.status();
            let body = response.text().await.unwrap_or_default();

            if resp_status.is_success() {
                // Parse response body to check for auto-acceptance
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("accepted")
                {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (plaintext)",
                        outgoing_id
                    );
                    let lender_request_id = parsed.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(db, &outgoing_id, &parsed, lender_request_id).await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Request sent", "status": "pending" })),
                )
                    .into_response()
            } else {
                // Parse lender response to check for auto-rejection
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("rejected")
                {
                    // Update outgoing request to rejected
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    let reason = parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (plaintext): {}",
                        outgoing_id,
                        reason
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": reason })),
                    )
                        .into_response();
                }
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
    use crate::models::book;

    let requests = crate::models::p2p_outgoing_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Look up local books by ISBN so we can link to them (same pattern as list_requests)
    let isbns: Vec<String> = requests
        .iter()
        .map(|(req, _)| req.book_isbn.clone())
        .filter(|isbn| !isbn.is_empty())
        .collect();

    let mut isbn_book_map: std::collections::HashMap<String, (i32, Option<String>)> =
        std::collections::HashMap::new();
    if !isbns.is_empty()
        && let Ok(books) = book::Entity::find()
            .filter(book::Column::Isbn.is_in(isbns))
            .all(&db)
            .await
    {
        for b in books {
            if let Some(isbn) = &b.isbn {
                isbn_book_map.insert(isbn.clone(), (b.id, b.cover_url.clone()));
            }
        }
    }

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            let book_info = isbn_book_map.get(&req.book_isbn);
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "book_id": book_info.map(|(id, _)| *id),
                "cover_url": book_info.and_then(|(_, url)| url.clone()),
                "status": req.status,
                "created_at": req.created_at,
                "updated_at": req.updated_at,
                "peer_id": peer.as_ref().map(|p| p.id),
                "peer_name": peer.as_ref().map(|p| p.name.clone()).unwrap_or("Unknown".to_string()),
                "peer_url": peer.map(|p| p.url)
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

/// Delete all non-pending outgoing requests (cleanup).
pub async fn clear_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use sea_orm::ConnectionTrait;
    let result = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM p2p_outgoing_requests WHERE status != 'pending'".to_owned(),
        ))
        .await;

    match result {
        Ok(r) => (
            StatusCode::OK,
            Json(json!({ "deleted": r.rows_affected() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Sync pending outgoing requests by querying each lender for current status.
///
/// For each pending outgoing request, sends a `request_status_query` via E2EE
/// (with relay fallback) to the lender. If the lender reports the request has been
/// accepted/rejected, updates the local outgoing request accordingly and creates
/// the borrowed copy if accepted.
pub async fn sync_outgoing_requests(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    let db = state.db();
    let pending = p2p_outgoing_request::Entity::find()
        .filter(p2p_outgoing_request::Column::Status.eq("pending"))
        .all(db)
        .await
        .unwrap_or_default();

    if pending.is_empty() {
        return (StatusCode::OK, Json(json!({ "synced": 0, "updated": 0 }))).into_response();
    }

    let mut synced = 0u32;
    let mut updated = 0u32;

    for outgoing in &pending {
        // Find the lender peer
        let lender = match peer::Entity::find_by_id(outgoing.to_peer_id).one(db).await {
            Ok(Some(p)) => p,
            _ => continue,
        };

        let query_payload = json!({
            "requester_request_id": outgoing.id,
        });

        // Try E2EE (with relay fallback)
        let result = try_send_e2ee(&state, &lender, "request_status_query", query_payload).await;
        synced += 1;

        if let Ok(Some(Some(ref clear_msg))) = result {
            let remote_status = clear_msg
                .payload
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");

            if remote_status != "pending" && remote_status != "not_found" {
                tracing::info!(
                    "Sync: outgoing request {} status changed to '{}'",
                    outgoing.id,
                    remote_status
                );

                if remote_status == "accepted" {
                    let lender_request_id =
                        clear_msg.payload.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(
                        db,
                        &outgoing.id,
                        &clear_msg.payload,
                        lender_request_id,
                    )
                    .await;
                } else {
                    // rejected or returned
                    let mut active: p2p_outgoing_request::ActiveModel = outgoing.clone().into();
                    active.status = Set(remote_status.to_string());
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    let _ = active.update(db).await;
                }
                updated += 1;
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "synced": synced, "updated": updated })),
    )
        .into_response()
}

/// Delete all non-pending incoming requests (cleanup).
pub async fn clear_incoming_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use sea_orm::ConnectionTrait;
    let result = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM p2p_requests WHERE status != 'pending'".to_owned(),
        ))
        .await;

    match result {
        Ok(r) => (
            StatusCode::OK,
            Json(json!({ "deleted": r.rows_affected() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
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

    // 2. Check copy availability before creating the request
    let has_available_copy = {
        use crate::models::book;
        use crate::models::copy;

        let book_found = book::Entity::find()
            .filter(book::Column::Isbn.eq(&payload.book_isbn))
            .one(&db)
            .await
            .unwrap_or(None);

        if let Some(b) = book_found {
            copy::Entity::find()
                .filter(copy::Column::BookId.eq(b.id))
                .filter(copy::Column::Status.eq("available"))
                .one(&db)
                .await
                .unwrap_or(None)
                .is_some()
        } else {
            false
        }
    };

    // 3. Check if auto-approve should be used
    let auto_approve =
        is_auto_approve_loans_enabled(&db).await && peer.connection_status == "accepted";

    // Determine initial status: auto-reject if no copy available
    let initial_status = if !has_available_copy {
        "rejected"
    } else {
        "pending"
    };

    // 4. Create Request Record
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = crate::models::p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set(initial_status.to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        requester_request_id: Set(payload.requester_request_id.clone()),
    };

    match crate::models::p2p_request::Entity::insert(request)
        .exec(&db)
        .await
    {
        Ok(_) => {
            // Auto-rejected: no available copy
            if !has_available_copy {
                tracing::info!(
                    "Auto-rejected loan request {} for '{}' - no available copy",
                    request_id,
                    payload.book_title
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "success": false, "status": "rejected", "reason": "no_available_copy" })),
                )
                    .into_response();
            }

            // If auto-approve is enabled, immediately accept the request
            if auto_approve {
                tracing::info!(
                    "Auto-approving loan request {} for peer {}",
                    request_id,
                    peer.name
                );
                match perform_loan_acceptance(
                    &db,
                    &request_id,
                    &payload.book_isbn,
                    &payload.book_title,
                    &peer,
                )
                .await
                {
                    Ok(result) => {
                        // Emit borrow_request notification (auto-approved)
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type: crate::domain::NotificationEventType::BorrowRequest,
                                title: payload.book_title.clone(),
                                body: Some(peer.name.clone()),
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(request_id.clone()),
                            },
                        )
                        .await;

                        // Fire-and-forget: try to notify borrower via E2EE (with relay fallback)
                        let confirm_payload = json!({
                            "isbn": result.book_isbn,
                            "title": result.book_title,
                            "cover_url": result.book_cover_url,
                            "lender_name": result.lender_name,
                            "due_date": result.due_date,
                            "request_id": request_id,
                            "requester_request_id": payload.requester_request_id,
                        });
                        let _ = try_send_e2ee(&state, &peer, "loan_confirmation", confirm_payload)
                            .await;

                        return (
                            StatusCode::OK,
                            Json(json!({
                                "success": true,
                                "status": "accepted",
                                "request_id": request_id,
                                "due_date": result.due_date,
                                "lender_name": result.lender_name,
                                "isbn": result.book_isbn,
                                "title": result.book_title,
                                "cover_url": result.book_cover_url,
                            })),
                        )
                            .into_response();
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Auto-approve failed for request {}: {} - staying pending",
                            request_id,
                            e
                        );
                        // Fall through to return "pending"
                    }
                }
            }

            // Emit borrow_request notification (only when NOT auto-approved)
            crate::services::notification_service::emit(
                &db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowRequest,
                    title: payload.book_title.clone(),
                    body: Some(peer.name.clone()),
                    ref_type: Some("peer".to_string()),
                    ref_id: Some(request_id.clone()),
                },
            )
            .await;

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
    use crate::models::book;

    let requests = crate::models::p2p_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Collect unique ISBNs to look up local books
    let isbns: Vec<String> = requests
        .iter()
        .map(|(req, _)| req.book_isbn.clone())
        .filter(|isbn| !isbn.is_empty())
        .collect();

    let mut isbn_book_map: std::collections::HashMap<String, (i32, Option<String>)> =
        std::collections::HashMap::new();
    if !isbns.is_empty()
        && let Ok(books) = book::Entity::find()
            .filter(book::Column::Isbn.is_in(isbns))
            .all(&db)
            .await
    {
        for b in books {
            if let Some(isbn) = &b.isbn {
                isbn_book_map.insert(isbn.clone(), (b.id, b.cover_url.clone()));
            }
        }
    }

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            let book_info = isbn_book_map.get(&req.book_isbn);
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "book_id": book_info.map(|(id, _)| *id),
                "cover_url": book_info.and_then(|(_, url)| url.clone()),
                "status": req.status,
                "created_at": req.created_at,
                "updated_at": req.updated_at,
                "peer_id": peer.as_ref().map(|p| p.id),
                "peer_name": peer.as_ref().map(|p| p.name.clone()).unwrap_or("Unknown".to_string()),
                "peer_url": peer.map(|p| p.url)
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
                // Peer no longer exists: auto-reject the request since we
                // cannot create the contact/loan without peer info.
                tracing::warn!(
                    "Peer {} not found for request {} - auto-rejecting",
                    req.from_peer_id,
                    req.id
                );
                active.status = Set("rejected".to_string());
                active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
                return (
                    StatusCode::OK,
                    Json(json!({ "message": "Request auto-rejected: peer no longer available" })),
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
                let new_contact =
                    contact::ActiveModel {
                        r#type: Set("Library".to_string()),
                        name: Set(peer.name.clone()),
                        library_owner_id: Set(
                            match crate::utils::library_helpers::resolve_library_id(&db).await {
                                Ok(id) => id,
                                Err(e) => {
                                    return (
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        Json(json!({ "error": format!("No library: {}", e) })),
                                    )
                                        .into_response();
                                }
                            },
                        ),
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
            library_id: Set(
                match crate::utils::library_helpers::resolve_library_id(&db).await {
                    Ok(id) => id,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("No library: {}", e) })),
                        )
                            .into_response();
                    }
                },
            ),
            loan_date: Set(chrono::Utc::now().to_rfc3339()),
            due_date: Set((chrono::Utc::now()
                + chrono::Duration::days(resolve_loan_duration_days(&db, book.id).await))
            .to_rfc3339()),
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
        let due_date = (chrono::Utc::now()
            + chrono::Duration::days(resolve_loan_duration_days(&db, book.id).await))
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
            "requester_request_id": req.requester_request_id,
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

        // 1. Find Peer/Contact (graceful: if peer is gone, still update request status)
        let peer_opt = peer::Entity::find_by_id(req.from_peer_id)
            .one(&db)
            .await
            .ok()
            .flatten();

        if peer_opt.is_none() {
            tracing::warn!(
                "Peer {} not found for return of request {} - updating request status only",
                req.from_peer_id,
                req.id
            );
        }

        let contact = match &peer_opt {
            Some(peer) => contact::Entity::find()
                .filter(contact::Column::Name.eq(&peer.name))
                .filter(contact::Column::Type.eq("Library"))
                .one(&db)
                .await
                .unwrap_or(None),
            None => None,
        };

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

                    // Emit book_returned notification
                    let peer_name = peer_opt
                        .as_ref()
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                    crate::services::notification_service::emit(
                        &db,
                        crate::domain::CreateNotification {
                            event_type: crate::domain::NotificationEventType::BookReturned,
                            title: book.title.clone(),
                            body: Some(peer_name),
                            ref_type: Some("loan".to_string()),
                            ref_id: Some(req.id.clone()),
                        },
                    )
                    .await;
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
                        && book.reading_status != "wanting"
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

            // Emit book_returned notification on borrower side
            if new_status == "returned" {
                let book_title = book::Entity::find()
                    .filter(book::Column::Isbn.eq(&book_isbn))
                    .one(&db)
                    .await
                    .ok()
                    .flatten()
                    .map(|b| b.title)
                    .unwrap_or_else(|| book_isbn.clone());
                let lender_name = if let Ok(Some(req)) =
                    p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await
                {
                    peer::Entity::find_by_id(req.to_peer_id)
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                        .map(|p| p.name)
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                crate::services::notification_service::emit(
                    &db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::BookReturned,
                        title: book_title,
                        body: Some(lender_name),
                        ref_type: Some("loan".to_string()),
                        ref_id: Some(id.clone()),
                    },
                )
                .await;
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
            // Fallback: delete the local copy + clean up orphaned book
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            cleanup_orphaned_book(&db, the_copy.book_id).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Copy deleted (no outgoing request found)" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("DB error finding outgoing request: {}", e);
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            cleanup_orphaned_book(&db, the_copy.book_id).await;
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
            cleanup_orphaned_book(&db, the_copy.book_id).await;
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
    cleanup_orphaned_book(&db, the_copy.book_id).await;

    (
        StatusCode::OK,
        Json(json!({ "message": "Book returned successfully" })),
    )
        .into_response()
}

/// Delete a book if it has no remaining copies, is not owned, and is not in the wishlist.
async fn cleanup_orphaned_book(db: &DatabaseConnection, book_id: i32) {
    use crate::models::{book, copy};
    if let Ok(Some(bk)) = book::Entity::find_by_id(book_id).one(db).await {
        let copy_count = copy::Entity::find()
            .filter(copy::Column::BookId.eq(bk.id))
            .count(db)
            .await
            .unwrap_or(1);

        let should_delete = !bk.owned && bk.reading_status != "wanting" && copy_count == 0;

        if should_delete {
            match book::Entity::delete_by_id(bk.id).exec(db).await {
                Ok(_) => tracing::info!("Deleted orphaned book {} after loan return", bk.id),
                Err(e) => tracing::error!("Failed to delete orphaned book {}: {}", bk.id, e),
            }
        } else {
            tracing::info!(
                "Book {} kept after loan return (owned={}, reading_status='{}', copies={})",
                bk.id,
                bk.owned,
                bk.reading_status,
                copy_count
            );
        }
    }
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
    /// Borrower's outgoing request ID (for precise confirmation matching)
    pub requester_request_id: Option<String>,
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
        "📚 Received loan confirmation: '{}' from {} (requester_request_id={:?})",
        payload.title,
        payload.lender_name,
        payload.requester_request_id
    );

    // Guard: verify a matching pending outgoing request exists.
    // This prevents stale relay messages from creating orphan borrowed copies.
    let has_matching_request = if let Some(ref rr_id) = payload.requester_request_id {
        // Precise match by borrower's outgoing request ID
        p2p_outgoing_request::Entity::find_by_id(rr_id)
            .filter(p2p_outgoing_request::Column::Status.eq("pending"))
            .one(&db)
            .await
            .ok()
            .flatten()
            .is_some()
    } else {
        // Backward compat: old confirmations without requester_request_id - match by ISBN
        let isbn_filter = payload.isbn.clone().unwrap_or_default();
        if !isbn_filter.is_empty() {
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                .filter(p2p_outgoing_request::Column::Status.eq("pending"))
                .one(&db)
                .await
                .ok()
                .flatten()
                .is_some()
        } else {
            // No ISBN, no requester_request_id - allow (best effort)
            true
        }
    };

    if !has_matching_request {
        tracing::warn!(
            "📚 No pending outgoing request for '{}' (requester_request_id={:?}, isbn={:?}), ignoring stale loan_confirmation",
            payload.title,
            payload.requester_request_id,
            payload.isbn
        );
        return (
            StatusCode::OK,
            Json(json!({ "message": "No pending request for this confirmation, ignored" })),
        )
            .into_response();
    }

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

    // 2. Idempotency: skip if a borrowed temporary copy already exists for this book
    let existing_borrowed = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("borrowed"))
        .filter(copy::Column::IsTemporary.eq(true))
        .one(&db)
        .await
        .ok()
        .flatten();

    if let Some(existing) = existing_borrowed {
        tracing::info!(
            "📚 Borrowed copy already exists (id={}) for book_id={}, skipping duplicate",
            existing.id,
            book_id
        );
        // Still update outgoing request if needed
        if let Some(ref lender_req_id) = payload.request_id {
            let outgoing = if let Some(ref rr_id) = payload.requester_request_id {
                p2p_outgoing_request::Entity::find_by_id(rr_id)
                    .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                    .one(&db)
                    .await
                    .ok()
                    .flatten()
            } else {
                let isbn_filter = payload.isbn.clone().unwrap_or_default();
                p2p_outgoing_request::Entity::find()
                    .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                    .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                    .one(&db)
                    .await
                    .ok()
                    .flatten()
            };
            if let Some(outgoing) = outgoing {
                let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
                active.lender_request_id = Set(Some(lender_req_id.clone()));
                active.status = Set("accepted".to_string());
                active.updated_at = Set(Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "message": "Loan already confirmed",
                "book_id": book_id,
                "copy_id": existing.id
            })),
        )
            .into_response();
    }

    // Create borrowed copy
    let now = Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(
            match crate::utils::library_helpers::resolve_library_id(&db).await {
                Ok(id) => id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("No library: {}", e) })),
                    )
                        .into_response();
                }
            },
        ),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {} jusqu'au {}",
            payload.lender_name, payload.due_date
        ))),
        acquisition_date: Set(Some(now.clone())),
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

            // Emit borrow_accepted notification
            crate::services::notification_service::emit(
                &db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowAccepted,
                    title: payload.title.clone(),
                    body: Some(payload.lender_name.clone()),
                    ref_type: Some("peer".to_string()),
                    ref_id: Some(c.id.to_string()),
                },
            )
            .await;

            // Store lender_request_id on the matching outgoing request
            if let Some(ref lender_req_id) = payload.request_id {
                let outgoing = if let Some(ref rr_id) = payload.requester_request_id {
                    p2p_outgoing_request::Entity::find_by_id(rr_id)
                        .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                } else {
                    let isbn_filter = payload.isbn.clone().unwrap_or_default();
                    p2p_outgoing_request::Entity::find()
                        .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                        .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                };
                if let Some(outgoing) = outgoing {
                    let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
                    active.lender_request_id = Set(Some(lender_req_id.clone()));
                    active.status = Set("accepted".to_string());
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    if let Err(e) = active.update(&db).await {
                        tracing::warn!(
                            "Failed to update outgoing request with lender_request_id: {}",
                            e
                        );
                    } else {
                        tracing::info!(
                            "✅ Outgoing request accepted, lender_request_id={}",
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

/// Save pre-fetched books to the local peer_books cache.
///
/// Called by Flutter after loading books via relay or live WiFi fetch,
/// so the Rust backend does not need to re-fetch from the remote peer.
/// Input: { "books": [{ "id": 5, "title": "...", ... }, ...] }
pub async fn cache_books_by_id(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // 1. Validate peer exists
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found: {}", peer_id) })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("DB error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Parse books array from payload
    let books: Vec<crate::models::Book> = match payload.get("books") {
        Some(books_val) => serde_json::from_value(books_val.clone()).unwrap_or_default(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'books' field" })),
            )
                .into_response();
        }
    };

    // 3-4. Upsert books cache (preserves first_seen_at)
    let count = upsert_peer_books_cache(&db, peer.id, None, books).await;

    (
        StatusCode::OK,
        Json(json!({ "count": count, "peer_id": peer_id })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Update peer display name
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdatePeerDisplayNameRequest {
    pub display_name: String,
}

/// Update a peer's user-defined display name.
pub async fn update_peer_display_name(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerDisplayNameRequest>,
) -> impl IntoResponse {
    let peer_opt = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    let peer_model = match peer_opt {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    let display_name = payload.display_name.trim().to_string();
    let mut active: peer::ActiveModel = peer_model.into();
    active.display_name = Set(if display_name.is_empty() {
        None
    } else {
        Some(display_name)
    });
    active.updated_at = Set(Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(updated) => (StatusCode::OK, Json(json!({ "peer": updated }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update display name: {}", e) })),
        )
            .into_response(),
    }
}
