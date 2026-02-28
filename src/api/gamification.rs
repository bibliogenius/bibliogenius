//! Gamification API handlers — thin wrappers delegating to gamification_service.
//!
//! All business logic lives in `services/gamification_service.rs`.
//! All DB access goes through `GamificationRepository` trait.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;

use crate::infrastructure::state::AppState;
use crate::services::gamification_service;

// Re-export types used by peer.rs for network sync (unchanged)
pub use gamification_service::{PublicGamificationStats, PublicTrackStats};

/// GET /api/user/status
pub async fn get_user_status(State(state): State<AppState>) -> impl IntoResponse {
    match gamification_service::get_user_status(state.gamification_repo.as_ref()).await {
        Ok(status) => (StatusCode::OK, Json(json!(status))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/gamification/public-stats
pub async fn get_public_stats(State(state): State<AppState>) -> impl IntoResponse {
    match gamification_service::get_public_stats(state.gamification_repo.as_ref()).await {
        Ok(Some(stats)) => (StatusCode::OK, Json(json!(stats))).into_response(),
        Ok(None) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Gamification stats sharing is disabled"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/gamification/leaderboard
pub async fn get_leaderboard(State(state): State<AppState>) -> impl IntoResponse {
    match gamification_service::build_leaderboard(state.gamification_repo.as_ref()).await {
        Ok(leaderboard) => (StatusCode::OK, Json(json!(leaderboard))).into_response(),
        Err(crate::domain::DomainError::Validation(msg)) => {
            (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/gamification/refresh-leaderboard
///
/// Syncs gamification stats from all connected peers, then returns the leaderboard.
/// NOTE: The peer sync logic in `peer::sync_peer_gamification_stats` is intentionally
/// kept unchanged (TNR-safe) — it still uses direct SeaORM via DatabaseConnection.
pub async fn refresh_leaderboard(State(state): State<AppState>) -> impl IntoResponse {
    use crate::models::{contact, peer};
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    };

    let db: &DatabaseConnection = state.db();

    // Check if network_gamification is enabled
    let network_enabled = match state
        .gamification_repo
        .is_module_enabled("network_gamification")
        .await
    {
        Ok(enabled) => enabled,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    if !network_enabled {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Network gamification is disabled"})),
        )
            .into_response();
    }

    // Fetch all connected peers and sync their gamification stats
    let peers = peer::Entity::find()
        .filter(peer::Column::ConnectionStatus.eq("accepted"))
        .all(db)
        .await
        .unwrap_or_default();

    let client = crate::api::peer::get_safe_client();

    // Sync all peers in parallel to avoid sequential timeouts
    let sync_futures: Vec<_> = peers
        .iter()
        .map(|p| {
            let client = client.clone();
            let peer_url = p.url.clone();
            let peer_name = p.name.clone();
            let peer_id = p.id;
            async move {
                let config_url = format!("{}/api/config", peer_url);
                match client.get(&config_url).send().await {
                    Ok(res) if res.status().is_success() => {
                        let config = match res.json::<crate::api::setup::ConfigResponse>().await {
                            Ok(c) => c,
                            Err(_) => return (peer_id, peer_url, peer_name, None),
                        };
                        (peer_id, peer_url, peer_name, Some(config))
                    }
                    _ => {
                        tracing::debug!(
                            "Peer {} unreachable during leaderboard refresh, keeping cached stats",
                            peer_url
                        );
                        (peer_id, peer_url, peer_name, None)
                    }
                }
            }
        })
        .collect();

    let results = futures::future::join_all(sync_futures).await;

    // Process results sequentially (DB writes are fast, no network)
    for (peer_id, peer_url, peer_name, config) in results {
        let Some(config) = config else { continue };

        // Update peer name and corresponding contact if it changed
        if config.library_name != peer_name
            && let Ok(Some(peer_model)) = peer::Entity::find_by_id(peer_id).one(db).await
        {
            let old_name = peer_model.name.clone();
            let mut active: peer::ActiveModel = peer_model.into();
            active.name = Set(config.library_name.clone());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(db).await;

            if let Ok(Some(contact_model)) = contact::Entity::find()
                .filter(contact::Column::Name.eq(&old_name))
                .filter(contact::Column::Type.eq("Library"))
                .one(db)
                .await
            {
                let mut contact_active: contact::ActiveModel = contact_model.into();
                contact_active.name = Set(config.library_name.clone());
                contact_active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                let _ = contact_active.update(db).await;
            }
        }

        // Sync peer gamification stats
        crate::api::peer::sync_peer_gamification_stats(
            db,
            peer_id,
            &peer_url,
            &client,
            Some(config.share_gamification_stats),
        )
        .await;
    }

    // Return the leaderboard via service
    match gamification_service::build_leaderboard(state.gamification_repo.as_ref()).await {
        Ok(leaderboard) => (StatusCode::OK, Json(json!(leaderboard))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Request body for updating gamification config
#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    pub reading_goal_yearly: Option<i32>,
    pub achievements_style: Option<String>,
}
