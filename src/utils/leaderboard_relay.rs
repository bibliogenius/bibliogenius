//! Utility for fetching leaderboard stats from relay peers (ADR-022, ADR-023).
//!
//! Provides three entry points:
//!
//! - [`build_local_stats_bundle`]: called by the relay responder to assemble
//!   our own public stats into a `PublicStatsBundle` for the remote peer.
//! - [`fetch_peer_public_stats_via_relay`]: called by each `refresh_leaderboard`
//!   handler to request stats from a non-LAN peer via the E2EE relay.
//! - [`notify_peers_of_stats_push`]: fire-and-forget push to all accepted peers
//!   when the local user beats their personal best (ADR-023).

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::infrastructure::AppState;
use crate::models::peer;
use crate::services::gamification_service;

/// Best score entry for a single mini-game (used in the leaderboard bundle).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameBestScoreEntry {
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
}

/// Bundle of public leaderboard stats for all four competitive features.
///
/// Returned by [`build_local_stats_bundle`] and deserialized from relay responses.
///
/// `enabled_modules` lists which modules are active on the remote peer.
/// A game score field is `None` either because the module is disabled
/// (check `enabled_modules`) or because no score exists yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicStatsBundle {
    pub share_gamification_stats: bool,
    /// Active modules on the remote peer (used to distinguish "disabled" from "no score yet").
    pub enabled_modules: Vec<String>,
    pub gamification: Option<gamification_service::PublicGamificationStats>,
    pub memory_game: Option<GameBestScoreEntry>,
    pub sliding_puzzle: Option<GameBestScoreEntry>,
    pub hangman: Option<GameBestScoreEntry>,
    /// Peer display name - included so relay-only peers stay up to date.
    pub library_name: Option<String>,
}

/// Assemble the local `PublicStatsBundle` in response to a `public_stats_request`.
///
/// Called from `relay_poller::handle_relay_request_response` when a remote peer
/// asks for our stats. Respects per-module opt-in settings.
pub async fn build_local_stats_bundle(state: &AppState) -> serde_json::Value {
    let db = state.db();

    // Read installation profile for enabled_modules and sharing flags
    let (enabled_modules, share_gamification_stats) =
        match crate::models::installation_profile::Entity::find_by_id(1)
            .one(db)
            .await
        {
            Ok(Some(p)) => {
                let mods: Vec<String> =
                    serde_json::from_str(&p.enabled_modules).unwrap_or_default();
                let share = mods.contains(&"share_gamification_stats".to_string());
                (mods, share)
            }
            _ => (vec![], false),
        };

    // Gamification stats (only if sharing enabled)
    let gamification = if share_gamification_stats {
        gamification_service::get_public_stats(state.gamification_repo.as_ref())
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    // Memory game best score
    let memory_game = if enabled_modules.contains(&"memory_game".to_string()) {
        use crate::modules::memory_game::domain::MemoryGameRepository;
        use crate::modules::memory_game::repository::SeaOrmGameRepository;
        SeaOrmGameRepository::new(db.clone())
            .get_best_score_entry()
            .await
            .ok()
            .flatten()
            .map(|e| GameBestScoreEntry {
                best_score: e.normalized_score,
                difficulty: e.difficulty,
                played_at: e.played_at,
            })
    } else {
        None
    };

    // Sliding puzzle best score
    let sliding_puzzle = if enabled_modules.contains(&"sliding_puzzle".to_string()) {
        use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
        use crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository;
        SeaOrmPuzzleRepository::new(db.clone())
            .get_best_score_entry()
            .await
            .ok()
            .flatten()
            .map(|e| GameBestScoreEntry {
                best_score: e.normalized_score,
                difficulty: e.difficulty,
                played_at: e.played_at,
            })
    } else {
        None
    };

    // Hangman best score
    let hangman = if enabled_modules.contains(&"hangman".to_string()) {
        use crate::modules::hangman::domain::HangmanRepository;
        use crate::modules::hangman::repository::SeaOrmHangmanRepository;
        SeaOrmHangmanRepository::new(db.clone())
            .get_best_score_entry()
            .await
            .ok()
            .flatten()
            .map(|e| GameBestScoreEntry {
                best_score: e.normalized_score,
                difficulty: e.difficulty,
                played_at: e.played_at,
            })
    } else {
        None
    };

    // Library name for peer display name updates
    let library_name: Option<String> = crate::models::library_config::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|lc| lc.name);

    json!({
        "share_gamification_stats": share_gamification_stats,
        "enabled_modules": enabled_modules,
        "gamification": gamification,
        "memory_game": memory_game,
        "sliding_puzzle": sliding_puzzle,
        "hangman": hangman,
        "library_name": library_name,
    })
}

/// Ensure a peer has complete relay credentials before a leaderboard relay sync.
///
/// Fast path: credentials already present → `Some(peer)` unchanged.
///
/// Slow path: `relay_write_token` is missing (peer connected before relay was
/// configured, or credentials never refreshed). Calls
/// [`crate::api::peer::refresh_peer_relay_credentials`] which tries LAN first
/// then falls back to the hub directory. On success the credentials are persisted
/// to the DB (so subsequent calls take the fast path) and the updated peer model
/// is returned.
///
/// Returns `None` when neither LAN nor hub can supply credentials.
pub async fn ensure_relay_credentials(
    db: &sea_orm::DatabaseConnection,
    peer: &peer::Model,
) -> Option<peer::Model> {
    // Fast path: already complete
    if peer.relay_url.is_some() && peer.mailbox_id.is_some() && peer.relay_write_token.is_some() {
        return Some(peer.clone());
    }
    // Missing write_token — attempt refresh.
    // refresh_peer_relay_credentials persists the result to the DB so future calls
    // take the fast path without a network round-trip.
    tracing::info!(
        "Leaderboard relay: peer '{}' missing relay_write_token, attempting credential refresh",
        peer.name
    );
    let (relay_url, mailbox_id, write_token) =
        crate::api::peer::refresh_peer_relay_credentials(db, peer).await?;
    let mut refreshed = peer.clone();
    refreshed.relay_url = Some(relay_url);
    refreshed.mailbox_id = Some(mailbox_id);
    refreshed.relay_write_token = Some(write_token);
    Some(refreshed)
}

/// Request leaderboard stats from a relay peer via the ADR-012 reply-to protocol.
///
/// Sends `public_stats_request` to the peer's relay mailbox. The relay poller
/// (accelerated by ADR-017 WS nudge) resolves the response in ~1-3 seconds.
///
/// Returns `None` if:
/// - The peer has no relay credentials
/// - The relay send fails (expired mailbox, etc.)
/// - The response times out (90 seconds)
pub async fn fetch_peer_public_stats_via_relay(
    state: &AppState,
    peer: &peer::Model,
) -> Option<PublicStatsBundle> {
    // Ensure the direct path is skipped inside try_send_e2ee.
    // We call this function only for peers whose direct HTTP already failed.
    state.mark_peer_direct_failed(peer.id);

    let result =
        crate::api::peer::try_send_e2ee(state, peer, "public_stats_request", json!({})).await;

    match result {
        Ok(Some(Some(clear_msg))) => {
            match serde_json::from_value::<PublicStatsBundle>(clear_msg.payload) {
                Ok(bundle) => {
                    tracing::info!(
                        "Leaderboard relay: received stats bundle from peer {}",
                        peer.id
                    );
                    Some(bundle)
                }
                Err(e) => {
                    tracing::warn!(
                        "Leaderboard relay: failed to parse stats bundle from peer {}: {}",
                        peer.id,
                        e
                    );
                    None
                }
            }
        }
        Ok(Some(None)) => {
            // Relay sent but no response (timeout)
            tracing::debug!(
                "Leaderboard relay: no response from peer {} (timeout or unreachable)",
                peer.id
            );
            None
        }
        Ok(None) => {
            // E2EE not available for this peer (missing keys)
            tracing::debug!("Leaderboard relay: E2EE not available for peer {}", peer.id);
            None
        }
        Err(e) => {
            tracing::warn!(
                "Leaderboard relay: relay send failed for peer {}: {}",
                peer.id,
                e
            );
            None
        }
    }
}

/// Send a fire-and-forget `public_stats_push` to all accepted peers via relay (ADR-023).
///
/// Builds the local `PublicStatsBundle` and sends it to every accepted peer that has
/// completed key exchange and has relay credentials. Modeled on
/// `catalog_notification::notify_peers_catalog_changed`.
///
/// Call this from a `tokio::spawn` to avoid blocking the game finish response path.
pub async fn notify_peers_of_stats_push(state: &AppState) {
    let db = state.db();

    // Crypto service is required for E2EE sends.
    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            tracing::debug!("Stats push: skipped - crypto service not ready");
            return;
        }
    };

    // Build the local stats bundle (respects sharing flags internally).
    let bundle_value = build_local_stats_bundle(state).await;

    // Load accepted peers with key exchange done.
    let peers = match peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .filter(peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Stats push: failed to load peers: {e}");
            return;
        }
    };

    if peers.is_empty() {
        tracing::debug!("Stats push: no eligible peers");
        return;
    }

    let message = crate::crypto::envelope::ClearMessage {
        message_type: "public_stats_push".to_string(),
        payload: bundle_value,
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let relay = crate::services::relay_transport::RelayTransport::new(Some(crypto_service));

    for p in &peers {
        let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
            (&p.relay_url, &p.mailbox_id, &p.relay_write_token)
        else {
            continue;
        };

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
            .send(relay_url, mailbox_id, write_token, &peer_x25519, &message)
            .await
        {
            Ok(()) => {
                tracing::info!("Stats push: notified peer {} ({})", p.name, p.id);
            }
            Err(crate::services::e2ee_transport::E2eeTransportError::PeerError(404, _)) => {
                tracing::info!(
                    "Stats push: peer {} mailbox expired (404), skipping",
                    p.name
                );
            }
            Err(e) => {
                tracing::warn!("Stats push: failed for peer {} ({}): {e}", p.name, p.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_stats_bundle_round_trips() {
        let bundle = PublicStatsBundle {
            share_gamification_stats: true,
            enabled_modules: vec![
                "memory_game".to_string(),
                "share_gamification_stats".to_string(),
            ],
            gamification: None,
            memory_game: Some(GameBestScoreEntry {
                best_score: 1250.0,
                difficulty: "medium".to_string(),
                played_at: "2026-04-10T12:00:00Z".to_string(),
            }),
            sliding_puzzle: None,
            hangman: None,
            library_name: Some("Alice's Library".to_string()),
        };

        let serialized = serde_json::to_value(&bundle).unwrap();
        let deserialized: PublicStatsBundle = serde_json::from_value(serialized).unwrap();

        assert!(deserialized.share_gamification_stats);
        assert!(deserialized.gamification.is_none());
        let mg = deserialized.memory_game.unwrap();
        assert_eq!(mg.best_score, 1250.0);
        assert_eq!(mg.difficulty, "medium");
        assert_eq!(deserialized.library_name.unwrap(), "Alice's Library");
    }

    #[test]
    fn public_stats_bundle_all_none_deserializes() {
        let json = serde_json::json!({
            "share_gamification_stats": false,
            "enabled_modules": [],
            "gamification": null,
            "memory_game": null,
            "sliding_puzzle": null,
            "hangman": null,
            "library_name": null,
        });
        let bundle: PublicStatsBundle = serde_json::from_value(json).unwrap();
        assert!(!bundle.share_gamification_stats);
        assert!(bundle.memory_game.is_none());
    }
}
