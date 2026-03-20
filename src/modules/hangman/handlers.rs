//! Hangman API handlers
//!
//! Handlers create their own repository from the DB connection.
//! No dependency on AppState beyond database access.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;

use super::domain::{HangmanRepository, HangmanResult};
use super::repository::SeaOrmHangmanRepository;
use super::service;
use crate::infrastructure::AppState;

/// Create a repository from AppState's DB connection
fn repo(state: &AppState) -> SeaOrmHangmanRepository {
    SeaOrmHangmanRepository::new(state.db().clone())
}

/// GET /api/game/hangman/difficulties
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
    #[serde(default)]
    pub exclude_book_ids: Vec<i32>,
}

/// POST /api/game/hangman/setup
pub async fn setup_game(
    State(state): State<AppState>,
    Json(payload): Json<SetupRequest>,
) -> impl IntoResponse {
    let difficulty = match service::HangmanDifficulty::parse(&payload.difficulty) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    match service::setup_game(&repo(&state), difficulty, &payload.exclude_book_ids).await {
        Ok(setup) => (StatusCode::OK, Json(setup)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/game/hangman/finish
pub async fn finish_game(
    State(state): State<AppState>,
    Json(result): Json<HangmanResult>,
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

/// GET /api/game/hangman/scores
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

/// GET /api/game/hangman/leaderboard
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

/// GET /api/game/hangman/public-best
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

/// Sync hangman scores from a single peer.
///
/// `peer_has_hangman`:
///   - `Some(true)`:  peer has hangman in enabled_modules -> fetch score
///   - `Some(false)`: peer does NOT have hangman -> delete cached scores
///   - `None`:        peer was unreachable (config unknown) -> preserve cache
pub(crate) async fn sync_peer_hangman_scores(
    db: &sea_orm::DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
    peer_name: &str,
    client: &reqwest::Client,
    peer_has_hangman: Option<bool>,
) {
    use super::domain::HangmanRepository;
    use super::repository::SeaOrmHangmanRepository;

    let game_repo = SeaOrmHangmanRepository::new(db.clone());

    match peer_has_hangman {
        None => {
            tracing::debug!(
                "Peer {} config unknown, preserving cached hangman scores",
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

    let url = format!("{}/api/game/hangman/public-best", peer_url);
    let response = match client.get(&url).send().await {
        Ok(res) if res.status().is_success() => res,
        _ => {
            tracing::warn!("Failed to fetch hangman score from peer {}", peer_url);
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
            tracing::warn!(
                "Failed to parse hangman score from peer {}: {}",
                peer_url,
                e
            );
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
            tracing::warn!("Failed to upsert peer hangman score: {}", e);
        } else {
            tracing::info!("Hangman score synced for peer {}", peer_id);
        }
    }
}

/// POST /api/game/hangman/refresh-leaderboard
pub async fn refresh_leaderboard(State(state): State<AppState>) -> impl IntoResponse {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = state.db();

    let local_enabled = match crate::models::installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
    {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"hangman".to_string())
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

    let peers = crate::models::peer::Entity::find()
        .filter(crate::models::peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
        .unwrap_or_default();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    for peer in &peers {
        let config_url = format!("{}/api/config", peer.url);
        let peer_has_hangman = match client.get(&config_url).send().await {
            Ok(res) if res.status().is_success() => {
                match res.json::<crate::api::setup::ConfigResponse>().await {
                    Ok(config) => Some(config.enabled_modules.contains(&"hangman".to_string())),
                    Err(_) => None,
                }
            }
            _ => None,
        };

        sync_peer_hangman_scores(
            db,
            peer.id,
            &peer.url,
            &peer.name,
            &client,
            peer_has_hangman,
        )
        .await;
    }

    get_leaderboard(State(state)).await.into_response()
}
