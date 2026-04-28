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
    /// Overall best score across every difficulty. Kept for backward
    /// compatibility with peers that only know this field. New peers
    /// should prefer `memory_scores_per_difficulty` when present.
    pub memory_game: Option<GameBestScoreEntry>,
    pub sliding_puzzle: Option<GameBestScoreEntry>,
    pub hangman: Option<GameBestScoreEntry>,
    /// One best entry per difficulty played. Empty for legacy peers; new
    /// peers receive the full set and can rebuild per-difficulty caches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_scores_per_difficulty: Vec<GameBestScoreEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub puzzle_scores_per_difficulty: Vec<GameBestScoreEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hangman_scores_per_difficulty: Vec<GameBestScoreEntry>,
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

    // Memory game best scores — overall best (for legacy peers) + one per difficulty.
    let (memory_game, memory_scores_per_difficulty) =
        if enabled_modules.contains(&"memory_game".to_string()) {
            use crate::modules::memory_game::domain::MemoryGameRepository;
            use crate::modules::memory_game::repository::SeaOrmGameRepository;
            let repo = SeaOrmGameRepository::new(db.clone());
            let overall =
                repo.get_best_score_entry()
                    .await
                    .ok()
                    .flatten()
                    .map(|e| GameBestScoreEntry {
                        best_score: e.normalized_score,
                        difficulty: e.difficulty,
                        played_at: e.played_at,
                    });
            let per_diff = repo
                .get_best_score_entries_per_difficulty()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| GameBestScoreEntry {
                    best_score: e.normalized_score,
                    difficulty: e.difficulty,
                    played_at: e.played_at,
                })
                .collect();
            (overall, per_diff)
        } else {
            (None, Vec::new())
        };

    // Sliding puzzle best scores — overall + per difficulty.
    let (sliding_puzzle, puzzle_scores_per_difficulty) =
        if enabled_modules.contains(&"sliding_puzzle".to_string()) {
            use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
            use crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository;
            let repo = SeaOrmPuzzleRepository::new(db.clone());
            let overall =
                repo.get_best_score_entry()
                    .await
                    .ok()
                    .flatten()
                    .map(|e| GameBestScoreEntry {
                        best_score: e.normalized_score,
                        difficulty: e.difficulty,
                        played_at: e.played_at,
                    });
            let per_diff = repo
                .get_best_score_entries_per_difficulty()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| GameBestScoreEntry {
                    best_score: e.normalized_score,
                    difficulty: e.difficulty,
                    played_at: e.played_at,
                })
                .collect();
            (overall, per_diff)
        } else {
            (None, Vec::new())
        };

    // Hangman best scores — overall + per difficulty.
    let (hangman, hangman_scores_per_difficulty) =
        if enabled_modules.contains(&"hangman".to_string()) {
            use crate::modules::hangman::domain::HangmanRepository;
            use crate::modules::hangman::repository::SeaOrmHangmanRepository;
            let repo = SeaOrmHangmanRepository::new(db.clone());
            let overall =
                repo.get_best_score_entry()
                    .await
                    .ok()
                    .flatten()
                    .map(|e| GameBestScoreEntry {
                        best_score: e.normalized_score,
                        difficulty: e.difficulty,
                        played_at: e.played_at,
                    });
            let per_diff = repo
                .get_best_score_entries_per_difficulty()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| GameBestScoreEntry {
                    best_score: e.normalized_score,
                    difficulty: e.difficulty,
                    played_at: e.played_at,
                })
                .collect();
            (overall, per_diff)
        } else {
            (None, Vec::new())
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
        "memory_scores_per_difficulty": memory_scores_per_difficulty,
        "puzzle_scores_per_difficulty": puzzle_scores_per_difficulty,
        "hangman_scores_per_difficulty": hangman_scores_per_difficulty,
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

/// Overall timeout for a single `public_stats_request` relay round-trip.
///
/// Tuned for the "contacts-style" UX: with ADR-017 WS nudge active, an
/// online peer responds in 1-3s. Anything longer almost certainly means
/// the peer is offline — we'd rather abort the spinner quickly and let
/// the user retry. Offline peers are cached in `peer_relay_failures`
/// so subsequent refreshes skip them entirely until the TTL expires.
const LEADERBOARD_RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Request leaderboard stats from a relay peer via the ADR-012 reply-to protocol.
///
/// Sends `public_stats_request` to the peer's relay mailbox. The relay poller
/// (accelerated by ADR-017 WS nudge) resolves the response in ~1-3 seconds.
///
/// Returns `None` if:
/// - The peer has no relay credentials
/// - The relay send fails (expired mailbox, etc.)
/// - The response times out ([`LEADERBOARD_RELAY_TIMEOUT`])
pub async fn fetch_peer_public_stats_via_relay(
    state: &AppState,
    peer: &peer::Model,
) -> Option<PublicStatsBundle> {
    // Ensure the direct path is skipped inside try_send_e2ee.
    // We call this function only for peers whose direct HTTP already failed.
    state.mark_peer_direct_failed(peer.id);

    let result = crate::api::peer::try_send_e2ee_with_timeout(
        state,
        peer,
        "public_stats_request",
        json!({}),
        LEADERBOARD_RELAY_TIMEOUT,
    )
    .await;

    match result {
        Ok(Some(Some(clear_msg))) => {
            match serde_json::from_value::<PublicStatsBundle>(clear_msg.payload) {
                Ok(bundle) => {
                    tracing::info!(
                        "Leaderboard relay: received stats bundle from peer {}",
                        peer.id
                    );
                    // Peer answered: clear any stale "unresponsive" mark.
                    state.clear_peer_relay_failed(peer.id);
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
            // Relay sent but no response (timeout). Remember so subsequent
            // refreshes skip this peer until TTL expires — matches the
            // contacts-side pattern of not re-probing known-offline peers.
            tracing::debug!(
                "Leaderboard relay: no response from peer {} (timeout or unreachable)",
                peer.id
            );
            state.mark_peer_relay_failed(peer.id);
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
            state.mark_peer_relay_failed(peer.id);
            None
        }
    }
}

/// Apply a [`PublicStatsBundle`] to all local game + gamification caches.
///
/// Upserts scores for enabled modules, optionally deletes cached scores for
/// disabled modules (set `clear_disabled` for the refresh path; leave false
/// for push notifications where the absence of a module just means "no update").
///
/// Also updates `peers.name` if the bundle carries a different display name.
///
/// Called by:
/// - [`sync_all_leaderboards`] (relay Phase 2 results)
/// - `handle_public_stats_push` in relay_poller (incoming push notifications)
pub async fn apply_stats_bundle_to_caches(
    db: &sea_orm::DatabaseConnection,
    peer_id: i32,
    peer_name: &str,
    bundle: &PublicStatsBundle,
    clear_disabled: bool,
) {
    let display_name = bundle.library_name.as_deref().unwrap_or(peer_name);

    // Update peer display name if changed
    if let Some(ref new_name) = bundle.library_name
        && !new_name.is_empty()
        && *new_name != peer_name
    {
        let _ = peer::Entity::update_many()
            .filter(peer::Column::Id.eq(peer_id))
            .col_expr(
                peer::Column::Name,
                sea_orm::sea_query::Expr::value(new_name.to_string()),
            )
            .col_expr(
                peer::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(chrono::Utc::now().to_rfc3339()),
            )
            .exec(db)
            .await;
    }

    // Memory game — prefer per-difficulty list when present, fall back to
    // the single overall-best entry from legacy peers.
    if bundle.enabled_modules.contains(&"memory_game".to_string()) {
        use crate::modules::memory_game::domain::MemoryGameRepository;
        let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
        if !bundle.memory_scores_per_difficulty.is_empty() {
            // Replace cached rows with the fresh per-difficulty set so
            // difficulties the peer no longer plays drop off naturally.
            let _ = repo.delete_peer_scores(peer_id).await;
            for entry in &bundle.memory_scores_per_difficulty {
                if entry.best_score <= 0.0 {
                    continue;
                }
                let _ = repo
                    .upsert_peer_score(
                        peer_id,
                        display_name,
                        entry.best_score,
                        &entry.difficulty,
                        &entry.played_at,
                    )
                    .await;
            }
        } else if let Some(entry) = &bundle.memory_game
            && entry.best_score > 0.0
        {
            let _ = repo
                .upsert_peer_score(
                    peer_id,
                    display_name,
                    entry.best_score,
                    &entry.difficulty,
                    &entry.played_at,
                )
                .await;
        }
    } else if clear_disabled {
        use crate::modules::memory_game::domain::MemoryGameRepository;
        let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
        let _ = repo.delete_peer_scores(peer_id).await;
    }

    // Sliding puzzle
    if bundle
        .enabled_modules
        .contains(&"sliding_puzzle".to_string())
    {
        use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
        let repo =
            crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
        if !bundle.puzzle_scores_per_difficulty.is_empty() {
            let _ = repo.delete_peer_scores(peer_id).await;
            for entry in &bundle.puzzle_scores_per_difficulty {
                if entry.best_score <= 0.0 {
                    continue;
                }
                let _ = repo
                    .upsert_peer_score(
                        peer_id,
                        display_name,
                        entry.best_score,
                        &entry.difficulty,
                        &entry.played_at,
                    )
                    .await;
            }
        } else if let Some(entry) = &bundle.sliding_puzzle
            && entry.best_score > 0.0
        {
            let _ = repo
                .upsert_peer_score(
                    peer_id,
                    display_name,
                    entry.best_score,
                    &entry.difficulty,
                    &entry.played_at,
                )
                .await;
        }
    } else if clear_disabled {
        use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
        let repo =
            crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
        let _ = repo.delete_peer_scores(peer_id).await;
    }

    // Hangman
    if bundle.enabled_modules.contains(&"hangman".to_string()) {
        use crate::modules::hangman::domain::HangmanRepository;
        let repo = crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
        if !bundle.hangman_scores_per_difficulty.is_empty() {
            let _ = repo.delete_peer_scores(peer_id).await;
            for entry in &bundle.hangman_scores_per_difficulty {
                if entry.best_score <= 0.0 {
                    continue;
                }
                let _ = repo
                    .upsert_peer_score(
                        peer_id,
                        display_name,
                        entry.best_score,
                        &entry.difficulty,
                        &entry.played_at,
                    )
                    .await;
            }
        } else if let Some(entry) = &bundle.hangman
            && entry.best_score > 0.0
        {
            let _ = repo
                .upsert_peer_score(
                    peer_id,
                    display_name,
                    entry.best_score,
                    &entry.difficulty,
                    &entry.played_at,
                )
                .await;
        }
    } else if clear_disabled {
        use crate::modules::hangman::domain::HangmanRepository;
        let repo = crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
        let _ = repo.delete_peer_scores(peer_id).await;
    }

    // Gamification
    if bundle.share_gamification_stats {
        if let Some(stats) = &bundle.gamification {
            use crate::models::peer_gamification_stats;

            let _ = peer_gamification_stats::Entity::delete_many()
                .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
                .exec(db)
                .await;

            let entry = peer_gamification_stats::ActiveModel {
                peer_id: sea_orm::Set(peer_id),
                library_name: sea_orm::Set(stats.library_name.clone()),
                collector_level: sea_orm::Set(stats.collector.level),
                collector_current: sea_orm::Set(stats.collector.current as i32),
                reader_level: sea_orm::Set(stats.reader.level),
                reader_current: sea_orm::Set(stats.reader.current as i32),
                lender_level: sea_orm::Set(stats.lender.level),
                lender_current: sea_orm::Set(stats.lender.current as i32),
                cataloguer_level: sea_orm::Set(stats.cataloguer.level),
                cataloguer_current: sea_orm::Set(stats.cataloguer.current as i32),
                synced_at: sea_orm::Set(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            };

            let _ = peer_gamification_stats::Entity::insert(entry)
                .exec(db)
                .await;
        }
    } else if clear_disabled {
        use crate::models::peer_gamification_stats;
        let _ = peer_gamification_stats::Entity::delete_many()
            .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
            .exec(db)
            .await;
    }
}

/// Sync leaderboard scores from all accepted peers for ALL games at once.
///
/// This is the single entry point for leaderboard refresh. Instead of each game
/// module syncing independently (3 relay round-trips per peer), this function
/// fetches one `PublicStatsBundle` per peer and distributes scores to all caches.
///
/// - `skip_direct`: skip Phase 1 direct HTTP (set `true` on cellular where LAN
///   peers are unreachable). Phase 2 relay is always attempted.
///
/// Waits for any in-progress sync to finish before running. This ensures
/// manual refresh always returns fresh data (unlike try_lock which would skip).
pub async fn sync_all_leaderboards(state: &AppState, skip_direct: bool) {
    let _guard = state.leaderboard_sync_lock().lock().await;

    let sync_start = std::time::Instant::now();
    let db = state.db();

    let peers = peer::Entity::find()
        .filter(peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
        .unwrap_or_default();

    tracing::info!(
        "leaderboard sync: {} accepted peer(s), skip_direct={}",
        peers.len(),
        skip_direct,
    );

    if peers.is_empty() {
        return;
    }

    // ── Phase 1: parallel direct HTTP ──────────────────────────────
    let mut relay_peers: Vec<crate::models::peer::Model> = Vec::new();
    let mut direct_ok = 0u32;

    // Phase 1 outcome per peer. Legacy calls and relay credential lookup
    // all happen *inside* the Phase 1 future to keep the whole phase
    // parallel across peers — the previous sequential post-processing
    // could spend 5s × 4 calls × N peers before Phase 2 even started.
    enum Phase1Outcome {
        Bundle(PublicStatsBundle),
        LegacyHandled,
        NeedsRelay(crate::models::peer::Model),
        Dead,
    }

    if !skip_direct {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        let phase1_futures: Vec<_> =
            peers
                .iter()
                .map(|p| {
                    let client = client.clone();
                    let peer = p.clone();
                    async move {
                        // Skip peers we've already proven unresponsive via
                        // relay within the TTL. Matches the contacts UX:
                        // known-offline peers don't keep blocking the spinner.
                        if state.is_peer_relay_unreachable(peer.id)
                            && state.is_peer_direct_unreachable(peer.id)
                        {
                            return Phase1Outcome::Dead;
                        }
                        // Skip peers already known to be unreachable — go
                        // straight to relay credential lookup.
                        if state.is_peer_direct_unreachable(peer.id) {
                            if state.is_peer_relay_unreachable(peer.id) {
                                return Phase1Outcome::Dead;
                            }
                            return match ensure_relay_credentials(db, &peer).await {
                                Some(ready) => Phase1Outcome::NeedsRelay(ready),
                                None => Phase1Outcome::Dead,
                            };
                        }

                        let bundle_url = format!("{}/api/public-stats-bundle", peer.url);
                        match client.get(&bundle_url).send().await {
                            Ok(res) if res.status().is_success() => {
                                match res.json::<PublicStatsBundle>().await {
                                    Ok(bundle) => Phase1Outcome::Bundle(bundle),
                                    Err(_) => Phase1Outcome::Dead,
                                }
                            }
                            Ok(res) if res.status() == reqwest::StatusCode::NOT_FOUND => {
                                // Legacy peer: /api/config + 4 per-game endpoints.
                                // Runs inside this future so multiple legacy peers
                                // don't serialize behind each other.
                                let config_url = format!("{}/api/config", peer.url);
                                let config = match client.get(&config_url).send().await {
                                    Ok(res) if res.status().is_success() => {
                                        res.json::<crate::api::setup::ConfigResponse>().await.ok()
                                    }
                                    _ => None,
                                };
                                let Some(config) = config else {
                                    if state.is_peer_relay_unreachable(peer.id) {
                                        return Phase1Outcome::Dead;
                                    }
                                    return match ensure_relay_credentials(db, &peer).await {
                                        Some(ready) => Phase1Outcome::NeedsRelay(ready),
                                        None => Phase1Outcome::Dead,
                                    };
                                };
                                let modules = &config.enabled_modules;
                                let (mem_enabled, puz_enabled, han_enabled) = (
                                    Some(modules.contains(&"memory_game".to_string())),
                                    Some(modules.contains(&"sliding_puzzle".to_string())),
                                    Some(modules.contains(&"hangman".to_string())),
                                );
                                // Parallel per-game fetches inside the legacy branch.
                                tokio::join!(
                                crate::modules::memory_game::handlers::sync_peer_memory_scores(
                                    db, peer.id, &peer.url, &peer.name, &client, mem_enabled,
                                ),
                                crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
                                    db, peer.id, &peer.url, &peer.name, &client, puz_enabled,
                                ),
                                crate::modules::hangman::handlers::sync_peer_hangman_scores(
                                    db, peer.id, &peer.url, &peer.name, &client, han_enabled,
                                ),
                                crate::api::peer::sync_peer_gamification_stats(
                                    db,
                                    peer.id,
                                    &peer.url,
                                    &client,
                                    Some(config.share_gamification_stats),
                                ),
                            );
                                Phase1Outcome::LegacyHandled
                            }
                            _ => {
                                if state.is_peer_relay_unreachable(peer.id) {
                                    return Phase1Outcome::Dead;
                                }
                                match ensure_relay_credentials(db, &peer).await {
                                    Some(ready) => Phase1Outcome::NeedsRelay(ready),
                                    None => Phase1Outcome::Dead,
                                }
                            }
                        }
                    }
                })
                .collect();

        let phase1_results = futures::future::join_all(phase1_futures).await;

        // Collect bundles to apply. apply_stats_bundle_to_caches is DB-bound
        // and cheap; running it sequentially here after join_all keeps the
        // SeaORM writes orderly without adding perceivable latency.
        for (peer, outcome) in peers.iter().zip(phase1_results) {
            match outcome {
                Phase1Outcome::Bundle(bundle) => {
                    direct_ok += 1;
                    apply_stats_bundle_to_caches(db, peer.id, &peer.name, &bundle, true).await;
                }
                Phase1Outcome::LegacyHandled => {
                    direct_ok += 1;
                }
                Phase1Outcome::NeedsRelay(ready) => {
                    relay_peers.push(ready);
                }
                Phase1Outcome::Dead => {}
            }
        }
    } else {
        // Skip direct entirely -- resolve relay credentials in parallel.
        let cred_futures: Vec<_> = peers
            .iter()
            .map(|peer| async move { ensure_relay_credentials(db, peer).await })
            .collect();
        for ready in futures::future::join_all(cred_futures)
            .await
            .into_iter()
            .flatten()
        {
            relay_peers.push(ready);
        }
    }

    tracing::info!(
        "leaderboard sync phase 1 done in {}ms: direct_ok={}, relay_queued={}",
        sync_start.elapsed().as_millis(),
        direct_ok,
        relay_peers.len(),
    );

    // ── Phase 2: relay fallback (parallel, per-peer timeout as safety net) ──
    if !relay_peers.is_empty() {
        let relay_start = std::time::Instant::now();
        let relay_count = relay_peers.len();
        // Safety net above the tighter `LEADERBOARD_RELAY_TIMEOUT` (25s)
        // enforced inside `try_send_e2ee_with_timeout`. 5s of headroom
        // covers any setup work around the actual await loop.
        let per_peer_timeout = LEADERBOARD_RELAY_TIMEOUT + std::time::Duration::from_secs(5);

        let relay_futures: Vec<_> = relay_peers
            .iter()
            .map(|peer| {
                let state = state.clone();
                let peer = peer.clone();
                async move {
                    let bundle = tokio::time::timeout(
                        per_peer_timeout,
                        fetch_peer_public_stats_via_relay(&state, &peer),
                    )
                    .await
                    .unwrap_or(None);
                    (peer, bundle)
                }
            })
            .collect();

        let relay_results = futures::future::join_all(relay_futures).await;

        let mut relay_ok = 0u32;
        let mut relay_no_response = 0u32;
        for (peer, bundle) in relay_results {
            let Some(bundle) = bundle else {
                relay_no_response += 1;
                continue;
            };
            relay_ok += 1;
            apply_stats_bundle_to_caches(db, peer.id, &peer.name, &bundle, true).await;
        }

        tracing::info!(
            "leaderboard sync phase 2 done in {}ms: relay_sent={}, relay_ok={}, relay_no_response={}",
            relay_start.elapsed().as_millis(),
            relay_count,
            relay_ok,
            relay_no_response,
        );
    }

    tracing::info!(
        "leaderboard sync completed in {}ms total",
        sync_start.elapsed().as_millis(),
    );
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
        // ADR-032: skip peers whose write_token has been flagged stale and is
        // still within the retry window, to avoid a broadcast-level 404 flood.
        if !p.relay_gate_allows_send() {
            continue;
        }
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
                // ADR-032: flag the peer's write_token as stale so the next
                // hour of broadcasts short-circuit at the gate, instead of
                // re-hammering the dead mailbox once per stats push.
                crate::api::peer::mark_peer_invite_stale(db, p.id).await;
                tracing::info!(
                    "Stats push: peer {} mailbox expired (404), flagged stale (ADR-032)",
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
            memory_scores_per_difficulty: vec![],
            puzzle_scores_per_difficulty: vec![],
            hangman_scores_per_difficulty: vec![],
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

    async fn test_state() -> AppState {
        let db = crate::db::init_db("sqlite::memory:")
            .await
            .expect("init_db in memory");
        AppState::new(db)
    }

    async fn insert_peer(db: &sea_orm::DatabaseConnection, name: &str) -> i32 {
        use sea_orm::ActiveModelTrait;
        use sea_orm::ActiveValue::Set;
        let now = chrono::Utc::now().to_rfc3339();
        let model = peer::ActiveModel {
            name: Set(name.to_string()),
            url: Set(format!("http://peer-{}.local", uuid::Uuid::new_v4())),
            key_exchange_done: Set(false),
            connection_status: Set("accepted".to_string()),
            auto_approve: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer");
        model.id
    }

    /// Regression test for the "stale peer name on LAN" bug: applying a bundle
    /// that carries a different `library_name` must update `peers.name` so the
    /// leaderboard UI shows the current display name without waiting for a
    /// relay round-trip.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_stats_bundle_updates_peer_name_when_different() {
        let state = test_state().await;
        let db = state.db();
        let peer_id = insert_peer(db, "Old Name").await;

        let bundle = PublicStatsBundle {
            share_gamification_stats: false,
            enabled_modules: vec![],
            gamification: None,
            memory_game: None,
            sliding_puzzle: None,
            hangman: None,
            memory_scores_per_difficulty: vec![],
            puzzle_scores_per_difficulty: vec![],
            hangman_scores_per_difficulty: vec![],
            library_name: Some("New Name".to_string()),
        };

        apply_stats_bundle_to_caches(db, peer_id, "Old Name", &bundle, true).await;

        let refreshed = peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .expect("query peer")
            .expect("peer exists");
        assert_eq!(refreshed.name, "New Name");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_stats_bundle_keeps_peer_name_when_bundle_name_missing() {
        let state = test_state().await;
        let db = state.db();
        let peer_id = insert_peer(db, "Stable Name").await;

        let bundle = PublicStatsBundle {
            share_gamification_stats: false,
            enabled_modules: vec![],
            gamification: None,
            memory_game: None,
            sliding_puzzle: None,
            hangman: None,
            memory_scores_per_difficulty: vec![],
            puzzle_scores_per_difficulty: vec![],
            hangman_scores_per_difficulty: vec![],
            library_name: None,
        };

        apply_stats_bundle_to_caches(db, peer_id, "Stable Name", &bundle, true).await;

        let refreshed = peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .expect("query peer")
            .expect("peer exists");
        assert_eq!(refreshed.name, "Stable Name");
    }

    /// The `/api/public-stats-bundle` handler must produce a payload that
    /// deserializes back into `PublicStatsBundle` with the expected shape,
    /// so LAN Phase 1 can consume it in a single round-trip.
    #[tokio::test(flavor = "multi_thread")]
    async fn public_stats_bundle_handler_returns_expected_fields() {
        use axum::extract::State;

        let state = test_state().await;
        let response = crate::api::public_stats::get_public_stats_bundle(State(state)).await;
        let value = response.0;

        // Shape check — these are the fields Phase 1 deserializes into `PublicStatsBundle`.
        assert!(value.get("share_gamification_stats").is_some());
        assert!(value.get("enabled_modules").is_some());
        assert!(value.get("memory_game").is_some());
        assert!(value.get("sliding_puzzle").is_some());
        assert!(value.get("hangman").is_some());
        assert!(value.get("library_name").is_some());

        let bundle: PublicStatsBundle = serde_json::from_value(value).expect("bundle deserializes");
        // Empty DB: no modules enabled, no scores, no gamification.
        assert!(bundle.enabled_modules.is_empty());
        assert!(bundle.memory_game.is_none());
        assert!(bundle.sliding_puzzle.is_none());
        assert!(bundle.hangman.is_none());
    }
}
