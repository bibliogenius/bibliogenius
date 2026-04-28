//! Peer catalog-change notification service.
//!
//! When the local user adds or deletes a book, call
//! `schedule_catalog_changed_notification(state)` to notify all accepted
//! peers that the catalog has changed. Peers receive a lightweight
//! `catalog_changed` E2EE relay message and can trigger a re-sync.
//!
//! ## Debounce (bulk-import safety)
//!
//! Notifications are rate-limited to at most one per `NOTIFY_COOLDOWN_SECS`
//! window (leading-edge). This prevents N relay HTTP requests when the user
//! imports a large backup (N books added in rapid succession). The peer
//! receives one notification, syncs, and gets the catalog state at that
//! point. Manual sync is available if the import is still in progress.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;

use crate::crypto::envelope::ClearMessage;
use crate::infrastructure::AppState;
use crate::models::peer;
use crate::services::relay_transport::RelayTransport;

/// Minimum interval between outgoing `catalog_changed` notifications.
/// Calls arriving within this window after a send are silently dropped.
const NOTIFY_COOLDOWN_SECS: u64 = 5;

/// Epoch-second timestamp of the last sent notification (0 = never sent).
static LAST_NOTIFY_SECS: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(0));

/// Schedule a `catalog_changed` notification to all accepted peers.
///
/// Returns immediately (non-blocking). The actual relay sends happen in a
/// background task. Safe to call from Axum handlers.
///
/// Calls within `NOTIFY_COOLDOWN_SECS` of the previous send are dropped to
/// prevent flooding the relay during bulk book imports.
pub fn schedule_catalog_changed_notification(state: AppState) {
    let now_secs = chrono::Utc::now().timestamp() as u64;
    let last = LAST_NOTIFY_SECS.load(Ordering::Relaxed);

    if now_secs.saturating_sub(last) < NOTIFY_COOLDOWN_SECS {
        tracing::debug!("Catalog notify: skipped (cooldown, last={last}s, now={now_secs}s)");
        return;
    }

    // Claim the slot with a compare-exchange to avoid a TOCTOU race between
    // two concurrent book mutations.
    if LAST_NOTIFY_SECS
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        // Another thread claimed the slot first — they will send the notification.
        tracing::debug!("Catalog notify: skipped (concurrent claim)");
        return;
    }

    tokio::spawn(async move {
        notify_peers_catalog_changed(state).await;
    });
}

/// Send `catalog_changed` to every accepted E2EE peer that has relay
/// credentials. Fire-and-forget per peer (individual failures are logged
/// but do not abort the loop).
async fn notify_peers_catalog_changed(state: AppState) {
    let db = state.db();

    // Crypto service is required for E2EE sends.
    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            tracing::warn!("Catalog notify: skipped — crypto service not ready");
            return;
        }
    };

    // Our library UUID is included in the payload so the recipient can match
    // the notification to the correct peer without a DB lookup.
    let library_uuid = match state.identity_service.library_uuid() {
        Some(uuid) => uuid.to_string(),
        None => {
            tracing::debug!("Catalog notify: skipped — library UUID not available");
            return;
        }
    };

    // Load accepted peers that have completed key exchange AND have relay
    // credentials (required to send even when on the same WiFi, because
    // this notification is always sent via relay to avoid blocking the
    // book mutation response path on a direct HTTP timeout).
    let peers = match peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .filter(peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Catalog notify: failed to load peers: {e}");
            return;
        }
    };

    if peers.is_empty() {
        tracing::info!("Catalog notify: no eligible peers (key_exchange_done=true + accepted)");
        return;
    }

    let message = ClearMessage {
        message_type: "catalog_changed".to_string(),
        payload: json!({ "library_uuid": library_uuid }),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let relay = RelayTransport::new(Some(crypto_service));

    for p in &peers {
        // ADR-032: skip peers flagged with a stale write_token inside the
        // retry window; otherwise every catalog_changed broadcast 404s them.
        if !p.relay_gate_allows_send() {
            tracing::debug!(
                "Catalog notify: peer {} write_token flagged stale (ADR-032), skipping",
                p.name
            );
            continue;
        }
        let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
            (&p.relay_url, &p.mailbox_id, &p.relay_write_token)
        else {
            tracing::warn!(
                "Catalog notify: peer {} has no relay credentials (relay_url/mailbox_id/write_token), skipping",
                p.name
            );
            continue;
        };

        // Parse peer's X25519 public key for encryption.
        let Some(x25519_hex) = &p.x25519_public_key else {
            tracing::warn!(
                "Catalog notify: peer {} missing x25519 key, skipping",
                p.name
            );
            continue;
        };
        let Ok(x_bytes) = hex::decode(x25519_hex) else {
            tracing::warn!(
                "Catalog notify: peer {} has invalid x25519 hex, skipping",
                p.name
            );
            continue;
        };
        if x_bytes.len() != 32 {
            tracing::warn!(
                "Catalog notify: peer {} x25519 key wrong length ({}), skipping",
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
                tracing::info!("Catalog notify: notified peer {} ({})", p.name, p.id);
            }
            Err(crate::services::e2ee_transport::E2eeTransportError::PeerError(404, _)) => {
                // ADR-032: flag the peer's write_token as stale so the next
                // hour of broadcasts short-circuit at the gate, instead of
                // re-hammering the dead mailbox once per change event.
                crate::api::peer::mark_peer_invite_stale(state.db(), p.id).await;
                tracing::info!(
                    "Catalog notify: peer {} mailbox expired (404), flagged stale (ADR-032)",
                    p.name
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Catalog notify: failed to notify peer {} ({}): {e}",
                    p.name,
                    p.id
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reset the global debounce counter between tests.
    fn reset_debounce() {
        LAST_NOTIFY_SECS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn debounce_allows_first_call() {
        reset_debounce();
        let now = chrono::Utc::now().timestamp() as u64;
        let last = LAST_NOTIFY_SECS.load(Ordering::Relaxed);
        assert!(
            now.saturating_sub(last) >= NOTIFY_COOLDOWN_SECS,
            "fresh state should pass the cooldown check"
        );
    }

    #[test]
    fn debounce_suppresses_rapid_second_call() {
        reset_debounce();
        let now = chrono::Utc::now().timestamp() as u64;
        // Simulate: first notification was sent 1 second ago.
        LAST_NOTIFY_SECS.store(now - 1, Ordering::Relaxed);

        let elapsed = now.saturating_sub(LAST_NOTIFY_SECS.load(Ordering::Relaxed));
        assert!(
            elapsed < NOTIFY_COOLDOWN_SECS,
            "call within cooldown should be suppressed (elapsed={elapsed}s)"
        );
    }

    #[test]
    fn debounce_allows_call_after_cooldown() {
        reset_debounce();
        let now = chrono::Utc::now().timestamp() as u64;
        // Simulate: last notification was sent NOTIFY_COOLDOWN_SECS + 1 seconds ago.
        let past = now.saturating_sub(NOTIFY_COOLDOWN_SECS + 1);
        LAST_NOTIFY_SECS.store(past, Ordering::Relaxed);

        let elapsed = now.saturating_sub(LAST_NOTIFY_SECS.load(Ordering::Relaxed));
        assert!(
            elapsed >= NOTIFY_COOLDOWN_SECS,
            "call after cooldown should be allowed (elapsed={elapsed}s)"
        );
    }

    /// ADR-032 wiring guard (canary for points 8 + the two sibling notifiers).
    ///
    /// `notify_peers_catalog_changed` must mark a peer as having a stale
    /// `relay_write_token` when the deposit returns 404 — this is what stops
    /// the production "165 deposits/day to a dead mailbox" loop. The three
    /// notifiers (catalog/profile/leaderboard) share the same wiring shape:
    /// regression here is a strong signal the same regression exists in the
    /// other two.
    #[tokio::test(flavor = "multi_thread")]
    async fn notify_peers_catalog_changed_marks_invite_stale_on_relay_404() {
        use crate::infrastructure::AppState;
        use crate::infrastructure::db::init_db;
        use crate::models::peer;
        use sea_orm::{ActiveModelTrait, EntityTrait, Set};
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Mock relay hub: every deposit POST is answered with 404, mirroring
        // a peer whose mailbox has been recreated/purged on the hub side.
        let relay = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/api/relay/mailbox/[^/]+/messages$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&relay)
            .await;

        let db = init_db("sqlite::memory:").await.expect("init test DB");
        let state = AppState::new(db.clone());
        // Initialize the local crypto identity so state.crypto_service()
        // returns Some on first access. Without this, the notifier exits
        // early before the relay POST and the test would not exercise the
        // 404 → mark_peer_invite_stale wiring at all.
        state
            .identity_service
            .init("test-library-uuid")
            .await
            .expect("init identity");

        // Generate a real x25519 public key for the peer so the encryption
        // step before the POST does not bail on a malformed key.
        let peer_secret = x25519_dalek::EphemeralSecret::random_from_rng(rand::thread_rng());
        let peer_pub = x25519_dalek::PublicKey::from(&peer_secret);
        let peer_x25519_hex = hex::encode(peer_pub.as_bytes());

        let now = chrono::Utc::now().to_rfc3339();
        let peer_id = peer::ActiveModel {
            name: Set("MockNithaM".to_string()),
            url: Set("relay://mock-mailbox-uuid".to_string()),
            library_uuid: Set(Some("peer-library-uuid".to_string())),
            x25519_public_key: Set(Some(peer_x25519_hex)),
            key_exchange_done: Set(true),
            connection_status: Set("accepted".to_string()),
            relay_url: Set(Some(relay.uri())),
            mailbox_id: Set(Some("dead-mailbox-uuid".to_string())),
            relay_write_token: Set(Some("write-token-that-points-to-nothing".to_string())),
            relay_write_token_invalid_at: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert peer");

        // Pre-condition: no stale flag.
        assert!(
            peer_id.relay_write_token_invalid_at.is_none(),
            "fresh peer must start with a clean ADR-032 flag",
        );

        super::notify_peers_catalog_changed(state).await;

        // Post-condition: deposit returned 404, the wiring set the flag.
        let reloaded = peer::Entity::find_by_id(peer_id.id)
            .one(&db)
            .await
            .expect("reload peer")
            .expect("peer exists");
        assert!(
            reloaded.relay_write_token_invalid_at.is_some(),
            "ADR-032 wiring (point 8) regressed: catalog notify did not flag the peer after a deposit 404",
        );
    }
}
