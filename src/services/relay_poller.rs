//! Background relay poller — periodically checks the relay hub for incoming messages.
//!
//! See SECURITY_GUIDELINES.md §B10 for polling jitter requirements.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::api::e2ee::{build_known_peers, dispatch_clear_message};
use crate::api::relay::get_my_relay_config;
use crate::infrastructure::AppState;
use crate::models::peer;
use crate::services::relay_transport::RelayTransport;

/// Start the background relay polling loop.
///
/// Polls at `interval` + random jitter (0-10s per B10) for incoming messages.
/// Each message is opened, dispatched through the standard E2EE pipeline,
/// then acknowledged (deleted from the relay).
pub async fn start_relay_polling(state: AppState, interval: Duration) {
    use rand::Rng;

    loop {
        // Jitter: 0-10 seconds (B10: prevent timing correlation)
        let jitter_ms = rand::thread_rng().gen_range(0..10_000);
        tokio::time::sleep(interval + Duration::from_millis(jitter_ms)).await;

        if let Err(e) = poll_once(&state).await {
            tracing::warn!("Relay poller: {e}");
        }
    }
}

/// Execute a single poll cycle.
async fn poll_once(state: &AppState) -> Result<(), String> {
    let db = state.db();

    // 1. Load my relay config
    let config = match get_my_relay_config(db).await {
        Some(c) => c,
        None => return Ok(()), // No relay configured, nothing to do
    };

    // 2. Get crypto service
    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => return Ok(()), // Identity not ready yet
    };

    // 3. Poll relay for pending messages
    let relay = RelayTransport::new(crypto_service.clone());
    let messages = relay
        .poll(&config.relay_url, &config.mailbox_uuid, &config.read_token)
        .await
        .map_err(|e| format!("poll failed: {e}"))?;

    if messages.is_empty() {
        return Ok(());
    }

    tracing::info!(
        "Relay poller: Received {} message(s) from relay",
        messages.len()
    );

    // 4. Load all E2EE-capable peers
    let peers = peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .all(db)
        .await
        .map_err(|e| format!("failed to load peers: {e}"))?;

    let (known_peers, peer_models) = build_known_peers(&peers);
    if known_peers.is_empty() {
        tracing::warn!("Relay poller: No known E2EE peers, cannot process messages");
        return Ok(());
    }

    // 5. Process each message
    for (message_id, envelope) in &messages {
        match process_relay_message(db, &crypto_service, envelope, &known_peers, &peer_models).await
        {
            Ok(()) => {
                // Acknowledge the message
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
            Err(e) => {
                tracing::warn!("Relay poller: Failed to process message {message_id}: {e}");
                // Don't ack — message will be retried on next poll
            }
        }
    }

    Ok(())
}

/// Process a single relay message through the existing E2EE pipeline.
async fn process_relay_message(
    db: &sea_orm::DatabaseConnection,
    crypto_service: &Arc<
        crate::services::crypto_service::CryptoService<
            crate::infrastructure::nonce_store::SqliteNonceStore,
        >,
    >,
    envelope: &crate::crypto::envelope::EncryptedEnvelope,
    known_peers: &[crate::services::crypto_service::PeerInfo],
    peer_models: &[peer::Model],
) -> Result<(), String> {
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

    // Dispatch through the shared handler (response is discarded for relay messages
    // since relay is fire-and-forget only)
    let _response = dispatch_clear_message(
        db,
        crypto_service,
        &clear_message,
        known_peers,
        peer_index,
        sender_peer,
    )
    .await;

    Ok(())
}
