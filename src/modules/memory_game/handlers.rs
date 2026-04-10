//! Memory Game API handlers
//!
//! Handlers create their own repository from the DB connection.
//! No dependency on AppState beyond database access.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;

use super::domain::{MemoryGameRepository, MemoryGameResult};
use super::repository::SeaOrmGameRepository;
use super::service;
use crate::infrastructure::AppState;

/// Create a repository from AppState's DB connection
fn repo(state: &AppState) -> SeaOrmGameRepository {
    SeaOrmGameRepository::new(state.db().clone())
}

/// GET /api/game/memory/difficulties
pub async fn available_difficulties(State(state): State<AppState>) -> impl IntoResponse {
    match service::available_difficulties(&repo(&state)).await {
        Ok(difficulties) => {
            let names: Vec<&str> = difficulties.iter().map(|d| d.as_str()).collect();
            (StatusCode::OK, Json(json!(names))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct SetupRequest {
    pub difficulty: String,
}

/// POST /api/game/memory/setup
pub async fn setup_game(
    State(state): State<AppState>,
    Json(payload): Json<SetupRequest>,
) -> impl IntoResponse {
    let difficulty = match service::MemoryDifficulty::parse(&payload.difficulty) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    match service::setup_game(&repo(&state), difficulty).await {
        Ok(cards) => (StatusCode::OK, Json(cards)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/game/memory/finish
pub async fn finish_game(
    State(state): State<AppState>,
    Json(result): Json<MemoryGameResult>,
) -> impl IntoResponse {
    match service::finish_game(&repo(&state), result).await {
        Ok(score) => (StatusCode::OK, Json(score)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/game/memory/scores
pub async fn get_top_scores(State(state): State<AppState>) -> impl IntoResponse {
    match repo(&state).get_top_scores(10).await {
        Ok(scores) => (StatusCode::OK, Json(scores)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/game/memory/leaderboard
pub async fn get_leaderboard(State(state): State<AppState>) -> impl IntoResponse {
    let r = repo(&state);
    let personal_best = r.get_personal_best().await.unwrap_or(None);
    let peer_scores = r.get_peer_scores().await.unwrap_or_default();

    (
        StatusCode::OK,
        Json(json!({
            "personal_best": personal_best,
            "peers": peer_scores,
        })),
    )
        .into_response()
}

/// GET /api/game/memory/public-best
/// Returns the best score entry with difficulty and played_at for peer leaderboards.
pub async fn get_public_best(State(state): State<AppState>) -> impl IntoResponse {
    match repo(&state).get_best_score_entry().await {
        Ok(Some(entry)) => (
            StatusCode::OK,
            Json(json!({
                "best_score": entry.normalized_score,
                "difficulty": entry.difficulty,
                "played_at": entry.played_at,
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::OK,
            Json(json!({
                "best_score": null,
                "difficulty": null,
                "played_at": null,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Sync memory game scores from a single peer.
///
/// `peer_has_memory_game`:
///   - `Some(true)`:  peer has memory_game in enabled_modules → fetch score
///   - `Some(false)`: peer does NOT have memory_game → delete cached scores
///   - `None`:        peer was unreachable (config unknown) → preserve cache
pub(crate) async fn sync_peer_memory_scores(
    db: &sea_orm::DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
    peer_name: &str,
    client: &reqwest::Client,
    peer_has_memory_game: Option<bool>,
) {
    use super::domain::MemoryGameRepository;
    use super::repository::SeaOrmGameRepository;

    let game_repo = SeaOrmGameRepository::new(db.clone());

    match peer_has_memory_game {
        None => {
            tracing::debug!(
                "Peer {} config unknown, preserving cached memory scores",
                peer_url
            );
            return;
        }
        Some(false) => {
            let _ = game_repo.delete_peer_scores(peer_id).await;
            return;
        }
        Some(true) => {}
    }

    // Fetch peer's public best score
    let url = format!("{}/api/game/memory/public-best", peer_url);
    let response = match client.get(&url).send().await {
        Ok(res) if res.status().is_success() => res,
        _ => {
            tracing::warn!("Failed to fetch memory score from peer {}", peer_url);
            return;
        }
    };

    #[derive(serde::Deserialize)]
    struct PublicBestResponse {
        best_score: Option<f64>,
        difficulty: Option<String>,
        played_at: Option<String>,
    }

    let data: PublicBestResponse = match response.json().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("Failed to parse memory score from peer {}: {}", peer_url, e);
            return;
        }
    };

    if let (Some(score), Some(difficulty), Some(played_at)) =
        (data.best_score, data.difficulty, data.played_at)
        && score > 0.0
    {
        if let Err(e) = game_repo
            .upsert_peer_score(peer_id, peer_name, score, &difficulty, &played_at)
            .await
        {
            tracing::warn!("Failed to upsert peer memory score: {}", e);
        } else {
            tracing::info!("Memory score synced for peer {}", peer_id);
        }
    }
}

/// Sync memory game scores from all accepted peers.
///
/// Phase 1: direct HTTP (LAN). Phase 2: relay fallback for non-LAN peers (ADR-022).
/// Called by both the HTTP refresh handler and the FFI path to avoid duplication.
pub(crate) async fn sync_all_peer_scores(state: &AppState) {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let db = state.db();

    let peers = crate::models::peer::Entity::find()
        .filter(crate::models::peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
        .unwrap_or_default();

    if peers.is_empty() {
        return;
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    // Phase 1: direct HTTP for all peers
    let mut relay_peers: Vec<crate::models::peer::Model> = Vec::new();
    for peer in &peers {
        let config_url = format!("{}/api/config", peer.url);
        let peer_has_memory_game = match client.get(&config_url).send().await {
            Ok(res) if res.status().is_success() => {
                match res.json::<crate::api::setup::ConfigResponse>().await {
                    Ok(config) => Some(config.enabled_modules.contains(&"memory_game".to_string())),
                    Err(_) => None,
                }
            }
            _ => None,
        };

        if peer_has_memory_game.is_none() {
            // Direct unreachable - try relay (ADR-022).
            // ensure_relay_credentials refreshes missing write_token from hub when needed.
            if let Some(ready) =
                crate::utils::leaderboard_relay::ensure_relay_credentials(db, peer).await
            {
                relay_peers.push(ready);
            } else {
                // No relay credentials available - preserve cached scores
                sync_peer_memory_scores(db, peer.id, &peer.url, &peer.name, &client, None).await;
            }
        } else {
            sync_peer_memory_scores(
                db,
                peer.id,
                &peer.url,
                &peer.name,
                &client,
                peer_has_memory_game,
            )
            .await;
        }
    }

    // Phase 2: relay fallback for non-LAN peers (ADR-022)
    if !relay_peers.is_empty() {
        let relay_futures: Vec<_> = relay_peers
            .iter()
            .map(|peer| {
                let state = state.clone();
                let peer = peer.clone();
                async move {
                    let bundle =
                        crate::utils::leaderboard_relay::fetch_peer_public_stats_via_relay(
                            &state, &peer,
                        )
                        .await;
                    (peer, bundle)
                }
            })
            .collect();

        let relay_results = futures::future::join_all(relay_futures).await;

        for (peer, bundle) in relay_results {
            let Some(bundle) = bundle else {
                // No response - preserve cached scores (same as direct timeout)
                continue;
            };
            if !bundle.enabled_modules.contains(&"memory_game".to_string()) {
                // Remote peer has disabled the module - clear cached scores
                let game_repo = SeaOrmGameRepository::new(db.clone());
                let _ = game_repo.delete_peer_scores(peer.id).await;
                continue;
            }
            let Some(entry) = bundle.memory_game else {
                continue;
            };
            if entry.best_score > 0.0 {
                let game_repo = SeaOrmGameRepository::new(db.clone());
                let display_name = bundle.library_name.as_deref().unwrap_or(&peer.name);
                if let Err(e) = game_repo
                    .upsert_peer_score(
                        peer.id,
                        display_name,
                        entry.best_score,
                        &entry.difficulty,
                        &entry.played_at,
                    )
                    .await
                {
                    tracing::warn!(
                        "Leaderboard relay: failed to upsert memory score for peer {}: {}",
                        peer.id,
                        e
                    );
                } else {
                    tracing::info!(
                        "Leaderboard relay: memory score synced for peer {} via relay",
                        peer.id
                    );
                }
            }
        }
    }
}

/// POST /api/game/memory/refresh-leaderboard
/// Fetches each accepted peer's memory game score and upserts into cache.
/// Falls back to relay (ADR-022) for peers unreachable via direct HTTP.
/// Returns the combined leaderboard.
pub async fn refresh_leaderboard(State(state): State<AppState>) -> impl IntoResponse {
    use sea_orm::EntityTrait;

    let db = state.db();

    // Check if memory_game module is enabled locally
    let local_enabled = match crate::models::installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
    {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"memory_game".to_string())
        }
        _ => false,
    };

    if !local_enabled {
        return (
            StatusCode::OK,
            Json(json!({"personal_best": null, "peers": []})),
        )
            .into_response();
    }

    sync_all_peer_scores(&state).await;

    // Return combined leaderboard
    get_leaderboard(State(state)).await.into_response()
}
