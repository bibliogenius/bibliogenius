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
pub async fn get_public_best(State(state): State<AppState>) -> impl IntoResponse {
    match repo(&state).get_personal_best().await {
        Ok(best) => (StatusCode::OK, Json(json!({"best_score": best}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
