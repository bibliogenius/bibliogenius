//! E2EE message sending with relay fallback, and relay credential refresh.

use super::*;
use crate::models::peer;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::json;

/// Default overall timeout for awaiting a relay response in `try_send_e2ee`.
///
/// 90s covers one full remote poller cycle (60s) plus jitter and processing,
/// so fire-and-forget request/response paths (loans, searches, syncs) keep
/// their historical behavior. Latency-sensitive callers (leaderboard refresh)
/// should use [`try_send_e2ee_with_timeout`] with a shorter bound.
pub(crate) const DEFAULT_E2EE_RELAY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(90);

/// Try to send a message to a peer via E2EE. Returns Ok(Some(response)) if E2EE succeeded,
/// Ok(None) if E2EE is not available for this peer (caller should fall back to plaintext).
///
/// ADR-012: All message types now support relay fallback. Request-response messages
/// (search_request, book_sync_request, library_*) attach reply_to fields so the
/// responder can deposit the answer in our mailbox.
///
/// Uses [`DEFAULT_E2EE_RELAY_TIMEOUT`] (90s) for relay response await. Use
/// [`try_send_e2ee_with_timeout`] when a different bound is needed.
pub(crate) async fn try_send_e2ee(
    state: &crate::infrastructure::AppState,
    peer: &peer::Model,
    message_type: &str,
    payload: serde_json::Value,
) -> Result<Option<Option<crate::crypto::envelope::ClearMessage>>, String> {
    try_send_e2ee_with_timeout(
        state,
        peer,
        message_type,
        payload,
        DEFAULT_E2EE_RELAY_TIMEOUT,
    )
    .await
}

/// Same as [`try_send_e2ee`] but with a caller-chosen `overall_timeout` for the
/// relay response await loop. Useful when the caller can tolerate missing a
/// slow peer in exchange for faster UI feedback (e.g. leaderboard refresh
/// where a 90s wait would freeze the refresh spinner).
pub async fn try_send_e2ee_with_timeout(
    state: &crate::infrastructure::AppState,
    peer: &peer::Model,
    message_type: &str,
    payload: serde_json::Value,
    overall_timeout: std::time::Duration,
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

    // Skip direct if peer recently failed and has usable relay credentials.
    // Don't skip when the write_token is gated by ADR-032, otherwise we'd
    // waste the one chance direct still has to reach the peer on LAN.
    let skip_direct = state.is_peer_direct_unreachable(peer.id)
        && peer.relay_url.is_some()
        && peer.mailbox_id.is_some()
        && peer.relay_write_token.is_some()
        && peer.relay_gate_allows_send();

    let direct_result = if skip_direct {
        tracing::info!(
            "E2EE: Skipping direct for peer {} (cached unreachable), using relay",
            peer.name,
        );
        Err(
            crate::services::e2ee_transport::E2eeTransportError::Network(
                "peer cached as unreachable".to_string(),
            ),
        )
    } else {
        transport
            .send(&peer.url, &peer_x25519, &peer_info, &message)
            .await
    };

    match direct_result {
        Ok(response) => {
            // Direct succeeded -- clear any cached failure for this peer
            state.clear_peer_direct_failed(peer.id);
            tracing::info!(
                "E2EE: Sent '{}' to peer {} ({})",
                message_type,
                peer.name,
                peer.id
            );
            Ok(Some(response))
        }
        Err(ref direct_err)
            if matches!(
                direct_err,
                crate::services::e2ee_transport::E2eeTransportError::Network(_)
            ) || direct_err.is_wrong_server_response() =>
        {
            let net_err = direct_err.to_string();
            // Mark peer as unreachable so subsequent calls skip direct.
            // Wrong-server responses (404/405/501: another service squats the
            // peer's host:port) count as unreachable too: the envelope never
            // reached a peer, so the relay fallback is duplicate-safe.
            if !skip_direct {
                state.mark_peer_direct_failed(peer.id);
            }
            // Peer unreachable in direct. Try relay fallback.
            // ADR-012: All message types can now be relayed. Request-response messages
            // attach reply_to fields so responses come back via our mailbox.
            // ADR-032: Skip relay entirely when the peer's write_token has been
            // flagged stale and the retry window hasn't elapsed. This is the
            // primary flood-suppression point for broadcast + interactive sends.
            if !peer.relay_gate_allows_send() {
                tracing::info!(
                    "E2EE Relay: Skipping peer {} - write_token flagged stale (ADR-032)",
                    peer.name
                );
                return Err(format!(
                    "E2EE: peer {} unreachable (direct: {net_err}, relay: invitation stale)",
                    peer.name
                ));
            }
            if let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
                (&peer.relay_url, &peer.mailbox_id, &peer.relay_write_token)
            {
                tracing::info!(
                    "E2EE: Direct failed ({}), trying relay for '{}' to peer {}",
                    net_err,
                    message_type,
                    peer.name,
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
                    "library_manifest_request",
                    "library_page_request",
                    "library_search_request",
                    "request_status_query",
                    "public_stats_request",  // ADR-022: leaderboard relay sync
                    "catalog_delta_request", // ADR-029: delta sync over relay
                    "avatar_sync_request",   // ADR-025: avatar + library_name sync over relay
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
                        ref _body,
                    )) => {
                        // Peer's mailbox expired/deleted on the hub.
                        // Try to refresh their relay credentials from /api/config.
                        tracing::warn!(
                            "E2EE Relay: Peer {} mailbox not found (404), attempting credential refresh",
                            peer.name,
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
                                    // ADR-032: still 404 after refreshed creds,
                                    // or any other terminal error. Flag the
                                    // write_token to stop the flood.
                                    mark_peer_invite_stale(state.db(), peer.id).await;
                                    if let Some(corr_id) = correlation_id_for_await {
                                        state.cancel_relay_request(&corr_id);
                                    }
                                    return Err(format!(
                                        "E2EE relay failed after credential refresh: {retry_err}"
                                    ));
                                }
                            }
                        } else {
                            tracing::warn!(
                                "E2EE Relay: Credential refresh returned nothing for peer {} (is_relay_only={})",
                                peer.name,
                                peer.url.starts_with("relay://")
                            );
                            // ADR-032: refresh exhausted LAN + hub directory;
                            // no way to recover the token without a fresh
                            // invitation. Flag and short-circuit next calls.
                            mark_peer_invite_stale(state.db(), peer.id).await;
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
                    // `overall_timeout` comes from the caller: legacy path uses
                    // `DEFAULT_E2EE_RELAY_TIMEOUT` (90s), latency-sensitive paths
                    // like leaderboard refresh use a shorter bound.
                    if let Some(corr_id) = correlation_id_for_await {
                        let mut rx = state.register_relay_request(corr_id.clone());
                        let start = std::time::Instant::now();

                        // Trigger immediate poll (don't wait for 60s background cycle)
                        let _ = crate::services::relay_poller::poll_once(
                            state,
                            crate::services::nudge_events::NudgeSource::Manual,
                        )
                        .await;

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
                                // Poll every 2s (was 5s) so responses are picked up faster
                                // when the WS nudge is unavailable (e.g. connection in progress).
                                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
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
                                    let _ = crate::services::relay_poller::poll_once(
                                        state,
                                        crate::services::nudge_events::NudgeSource::Manual,
                                    )
                                    .await;
                                }
                            }
                        }
                    }

                    return Ok(Some(None));
                }
            }

            tracing::warn!(
                "E2EE: Peer {} unreachable via LAN ({}) and has no relay credentials",
                peer.name,
                net_err,
            );
            Err(format!("E2EE send failed: network error: {net_err}"))
        }
        Err(e) => Err(format!("E2EE send failed: {e}")),
    }
}

/// Pull a peer's avatar and library name over E2EE (ADR-025).
///
/// Sends `avatar_sync_request`, waits for `avatar_sync_response`, persists
/// `peers.avatar_config` and `peers.name` when they differ from the cached
/// value. Returns `true` when at least one field changed.
///
/// Called from three trigger points:
///   1. On first-seen of an accepted relay-only peer (no cached avatar).
///   2. From Flutter after receiving a `profile_changed` WS nudge.
///   3. Opportunistically during relay poll cycles (at most once per 24h).
pub(crate) async fn try_pull_avatar_via_relay(
    state: &crate::infrastructure::AppState,
    peer_id: i32,
) -> Result<bool, String> {
    let db = state.db();

    let peer_model = peer::Entity::find_by_id(peer_id)
        .one(db)
        .await
        .map_err(|e| format!("load peer {peer_id}: {e}"))?
        .ok_or_else(|| format!("peer {peer_id} not found"))?;

    let send_result = try_send_e2ee(state, &peer_model, "avatar_sync_request", json!({})).await;

    let response = match send_result {
        Ok(Some(Some(resp))) => resp,
        Ok(Some(None)) => {
            tracing::info!(
                "avatar_sync: peer {} did not respond (likely pre-ADR-025)",
                peer_model.name
            );
            return Ok(false);
        }
        Ok(None) => {
            tracing::debug!(
                "avatar_sync: peer {} has no E2EE capability",
                peer_model.name
            );
            return Ok(false);
        }
        Err(e) => return Err(format!("try_send_e2ee: {e}")),
    };

    // `avatar_config` is either a JSON object/value or null. Serialize back
    // to a string for storage (peer.avatar_config is TEXT, matching the
    // existing piggyback path in sync_peer).
    let new_avatar: Option<String> = response
        .payload
        .get("avatar_config")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::to_string(v).ok());

    let new_name: Option<String> = response
        .payload
        .get("library_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let avatar_changed = new_avatar.is_some() && new_avatar != peer_model.avatar_config;
    let name_changed = new_name.as_ref().is_some_and(|n| n != &peer_model.name);

    if !avatar_changed && !name_changed {
        tracing::debug!("avatar_sync: peer {} already up to date", peer_model.name);
        return Ok(false);
    }

    let mut active: peer::ActiveModel = peer_model.clone().into();
    if avatar_changed {
        active.avatar_config = Set(new_avatar.clone());
    }
    if name_changed {
        active.name = Set(new_name.clone().unwrap_or_else(|| peer_model.name.clone()));
    }
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    active
        .update(db)
        .await
        .map_err(|e| format!("update peer {peer_id}: {e}"))?;

    tracing::info!(
        "avatar_sync: peer {} updated (avatar_changed={}, name_changed={})",
        peer_model.name,
        avatar_changed,
        name_changed
    );
    Ok(true)
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
pub async fn refresh_peer_relay_credentials(
    db: &DatabaseConnection,
    peer_model: &peer::Model,
) -> Option<(String, String, String)> {
    let (relay_url, mailbox_id, write_token) = if peer_model.url.starts_with("relay://") {
        // Relay-only: query hub directory for updated credentials
        refresh_via_hub(db, peer_model).await?
    } else {
        // LAN peer: try direct HTTP fetch first (fast, no hub dependency)
        let lan_result = refresh_via_lan(peer_model).await;
        if lan_result.is_some() {
            lan_result?
        } else if peer_model.library_uuid.is_some() {
            // LAN unreachable -- fallback to hub directory if peer is registered
            tracing::info!(
                "Relay: LAN refresh failed for peer '{}', falling back to hub directory",
                peer_model.name
            );
            refresh_via_hub(db, peer_model).await?
        } else {
            return None;
        }
    };

    if relay_url.is_empty() || mailbox_id.is_empty() || write_token.is_empty() {
        return None;
    }

    // Update peer record with fresh relay credentials. Any stale-invite flag
    // (ADR-032) is cleared at the same time since the new token is assumed
    // fresh from the hub/LAN probe.
    if let Ok(Some(existing)) = peer::Entity::find_by_id(peer_model.id).one(db).await {
        let mut active: peer::ActiveModel = existing.into();
        active.relay_url = Set(Some(relay_url.clone()));
        active.mailbox_id = Set(Some(mailbox_id.clone()));
        active.relay_write_token = Set(Some(write_token.clone()));
        active.relay_write_token_invalid_at = Set(None);
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

/// ADR-032: Flag a peer's `relay_write_token` as invalid. Called after a
/// deposit 404 that could not be recovered by `refresh_peer_relay_credentials`.
/// Subsequent sends short-circuit via `peer.relay_gate_allows_send()` until
/// either the retry window elapses, a refresh succeeds, or the user imports
/// a fresh invitation from the peer.
pub(crate) async fn mark_peer_invite_stale(db: &DatabaseConnection, peer_id: i32) {
    if let Ok(Some(existing)) = peer::Entity::find_by_id(peer_id).one(db).await {
        let mut active: peer::ActiveModel = existing.into();
        active.relay_write_token_invalid_at = Set(Some(chrono::Utc::now().to_rfc3339()));
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        if let Err(e) = active.update(db).await {
            tracing::warn!(
                "Relay: failed to persist stale-invite flag for peer {}: {}",
                peer_id,
                e
            );
        } else {
            tracing::info!(
                "Relay: Flagged peer {} write_token as stale (ADR-032)",
                peer_id
            );
        }
    }
}

/// Refresh relay credentials via direct HTTP to the peer's LAN URL.
async fn refresh_via_lan(peer_model: &peer::Model) -> Option<(String, String, String)> {
    let client = get_safe_client();
    let config_url = format!("{}/api/config", peer_model.url.trim_end_matches('/'));
    tracing::debug!(
        "Relay: LAN refresh attempt for peer '{}' at {}",
        peer_model.name,
        config_url
    );

    let response = match client.get(&config_url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                "Relay: LAN refresh failed for peer '{}': {}",
                peer_model.name,
                e
            );
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::debug!(
            "Relay: LAN refresh for peer '{}' returned HTTP {}",
            peer_model.name,
            response.status()
        );
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
    tracing::info!(
        "Relay: Hub refresh attempt for peer '{}' (node_id: {:?})",
        peer_model.name,
        peer_model.library_uuid
    );
    let hub_url =
        crate::services::hub_directory_service::HubDirectoryService::hub_base_url().ok()?;
    let peer_node_id = peer_model.library_uuid.as_deref()?;

    // Authenticate with our own write_token
    let our_config = crate::services::hub_directory_service::HubDirectoryService::get_config(db)
        .await
        .ok()
        .flatten()?;

    let client = get_safe_client();
    let url = format!("{hub_url}/api/directory/{peer_node_id}");
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

#[cfg(test)]
mod stale_invite_flag_tests {
    //! ADR-032: `relay_write_token_invalid_at` gate + clear paths.
    use super::*;
    use crate::db;
    use sea_orm::Set;

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer_with_relay(db: &DatabaseConnection) -> peer::Model {
        let now = chrono::Utc::now().to_rfc3339();
        let active = peer::ActiveModel {
            name: Set("test-peer".to_string()),
            url: Set("http://test-peer.local:8080".to_string()),
            relay_url: Set(Some("https://hub.example.org".to_string())),
            mailbox_id: Set(Some("mbx-original".to_string())),
            relay_write_token: Set(Some("wtok-original".to_string())),
            relay_write_token_invalid_at: Set(None),
            key_exchange_done: Set(true),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        let id = peer::Entity::insert(active)
            .exec(db)
            .await
            .expect("insert peer")
            .last_insert_id;
        peer::Entity::find_by_id(id)
            .one(db)
            .await
            .expect("query")
            .expect("peer row")
    }

    /// `mark_peer_invite_stale` persists a non-empty timestamp so later
    /// queries see the gate closed.
    #[tokio::test]
    async fn mark_peer_invite_stale_sets_timestamp() {
        let db = setup_db().await;
        let p = insert_peer_with_relay(&db).await;
        assert!(p.relay_write_token_invalid_at.is_none());

        mark_peer_invite_stale(&db, p.id).await;

        let reloaded = peer::Entity::find_by_id(p.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.relay_write_token_invalid_at.is_some(),
            "timestamp must be persisted after mark_peer_invite_stale"
        );
        assert!(
            !reloaded.relay_gate_allows_send(),
            "fresh flag must close the retry gate"
        );
    }

    /// Gate admits a send again after the retry window has elapsed, so a
    /// peer coming back online eventually recovers without user action.
    #[tokio::test]
    async fn gate_admits_send_after_retry_window() {
        let db = setup_db().await;
        let mut p = insert_peer_with_relay(&db).await;

        let one_hour_ago = (chrono::Utc::now() - chrono::Duration::seconds(3601)).to_rfc3339();
        p.relay_write_token_invalid_at = Some(one_hour_ago);
        assert!(
            p.relay_gate_allows_send(),
            "gate must admit a send once the retry window has elapsed"
        );

        p.relay_write_token_invalid_at = Some(chrono::Utc::now().to_rfc3339());
        assert!(
            !p.relay_gate_allows_send(),
            "gate must close again when the timestamp is fresh"
        );
    }

    /// `refresh_peer_relay_credentials` shouldn't be callable here without a
    /// real peer HTTP endpoint, so we simulate the credential write path: a
    /// successful update must clear any previously set stale-invite flag.
    #[tokio::test]
    async fn refresh_clears_stale_invite_flag() {
        let db = setup_db().await;
        let p = insert_peer_with_relay(&db).await;
        mark_peer_invite_stale(&db, p.id).await;

        // Emulate the credential-write branch inside refresh_peer_relay_credentials
        let existing = peer::Entity::find_by_id(p.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut active: peer::ActiveModel = existing.into();
        active.relay_write_token = Set(Some("wtok-fresh".to_string()));
        active.relay_write_token_invalid_at = Set(None);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(&db).await.expect("update peer");

        let reloaded = peer::Entity::find_by_id(p.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.relay_write_token_invalid_at.is_none(),
            "refresh must clear stale-invite flag"
        );
        assert!(reloaded.relay_gate_allows_send());
    }
}
