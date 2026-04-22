//! Profile-change notification service (ADR-025).
//!
//! When the local user edits their profile (avatar today, more fields in the
//! future), call `schedule_profile_changed_notification(state, &["avatar"])`
//! to push a lightweight `profile_changed` E2EE relay message to all
//! accepted peers. Peers receive the nudge and pull the fresh profile via
//! `avatar_sync_request` (see ADR-025 §3).
//!
//! A dedicated message type is used instead of piggybacking on
//! `catalog_changed` so receivers do not trigger a costly delta catalog sync
//! on every avatar edit (ADR-029 would otherwise kick in).
//!
//! Debouncing is intentionally looser than `catalog_notification` because
//! profile edits are user-driven (no bulk-import equivalent), but we still
//! swallow bursts within a short window in case the UI emits multiple
//! saves back-to-back (avatar picker + library-name edit, etc.).

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

use crate::crypto::envelope::ClearMessage;
use crate::infrastructure::AppState;
use crate::models::peer;
use crate::services::relay_transport::RelayTransport;

const NOTIFY_COOLDOWN_SECS: u64 = 3;

static LAST_NOTIFY_SECS: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// Notify all accepted peers that one or more of our profile fields changed.
///
/// Returns immediately. Relay sends happen in a background task. Safe to
/// call from Axum handlers after the DB write has committed.
pub fn schedule_profile_changed_notification(state: AppState, changed: Vec<String>) {
    let now_secs = chrono::Utc::now().timestamp() as u64;
    let last = LAST_NOTIFY_SECS.load(Ordering::Relaxed);

    if now_secs.saturating_sub(last) < NOTIFY_COOLDOWN_SECS {
        tracing::debug!("Profile notify: skipped (cooldown, last={last}s, now={now_secs}s)");
        return;
    }

    if LAST_NOTIFY_SECS
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        tracing::debug!("Profile notify: skipped (concurrent claim)");
        return;
    }

    tokio::spawn(async move {
        notify_peers_profile_changed(state, changed).await;
    });
}

async fn notify_peers_profile_changed(state: AppState, changed: Vec<String>) {
    let db = state.db();

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            tracing::warn!("Profile notify: skipped — crypto service not ready");
            return;
        }
    };

    let peers = match peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .filter(peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Profile notify: failed to load peers: {e}");
            return;
        }
    };

    if peers.is_empty() {
        tracing::info!("Profile notify: no eligible peers (key_exchange_done=true + accepted)");
        return;
    }

    let message = ClearMessage {
        message_type: "profile_changed".to_string(),
        payload: json!({ "changed": changed }),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let relay = RelayTransport::new(Some(crypto_service));

    for p in &peers {
        // ADR-032: don't broadcast to peers whose write_token has been flagged
        // stale and is still within the retry window.
        if !p.relay_gate_allows_send() {
            tracing::debug!(
                "Profile notify: peer {} write_token flagged stale (ADR-032), skipping",
                p.name
            );
            continue;
        }
        let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
            (&p.relay_url, &p.mailbox_id, &p.relay_write_token)
        else {
            tracing::debug!(
                "Profile notify: peer {} has no relay credentials, skipping",
                p.name
            );
            continue;
        };

        let Some(x25519_hex) = &p.x25519_public_key else {
            tracing::warn!(
                "Profile notify: peer {} missing x25519 key, skipping",
                p.name
            );
            continue;
        };
        let Ok(x_bytes) = hex::decode(x25519_hex) else {
            tracing::warn!(
                "Profile notify: peer {} has invalid x25519 hex, skipping",
                p.name
            );
            continue;
        };
        if x_bytes.len() != 32 {
            tracing::warn!(
                "Profile notify: peer {} x25519 key wrong length ({}), skipping",
                p.name,
                x_bytes.len()
            );
            continue;
        }
        let x_arr: [u8; 32] = x_bytes.try_into().unwrap();
        let peer_x25519 = x25519_dalek::PublicKey::from(x_arr);

        match relay
            .send(relay_url, mailbox_id, write_token, &peer_x25519, &message)
            .await
        {
            Ok(()) => {
                tracing::info!("Profile notify: notified peer {} ({})", p.name, p.id);
            }
            Err(crate::services::e2ee_transport::E2eeTransportError::PeerError(404, _)) => {
                tracing::info!(
                    "Profile notify: peer {} mailbox expired (404), skipping",
                    p.name
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Profile notify: failed to notify peer {} ({}): {e}",
                    p.name,
                    p.id
                );
            }
        }
    }
}
