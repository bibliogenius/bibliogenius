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

    // Process direct results sequentially (DB writes are fast, no network).
    // Collect peers that failed direct HTTP and have relay credentials for the relay pass.
    let mut relay_peers: Vec<peer::Model> = Vec::new();
    for (peer_id, peer_url, peer_name, config) in results {
        if let Some(config) = config {
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
        } else {
            // Peer unreachable via direct HTTP - try relay if credentials are available (ADR-022)
            if let Some(peer_model) = peers.iter().find(|p| p.id == peer_id)
                && peer_model.relay_url.is_some()
                && peer_model.mailbox_id.is_some()
                && peer_model.relay_write_token.is_some()
            {
                relay_peers.push(peer_model.clone());
            }
        }
    }

    // ADR-022: relay pass - fetch gamification stats from non-LAN peers in parallel
    if !relay_peers.is_empty() {
        use crate::models::peer_gamification_stats;

        let relay_futures: Vec<_> = relay_peers
            .iter()
            .map(|peer_model| {
                let state = state.clone();
                let peer_model = peer_model.clone();
                async move {
                    let bundle =
                        crate::utils::leaderboard_relay::fetch_peer_public_stats_via_relay(
                            &state,
                            &peer_model,
                        )
                        .await;
                    (peer_model, bundle)
                }
            })
            .collect();

        let relay_results = futures::future::join_all(relay_futures).await;

        for (peer_model, bundle) in relay_results {
            let Some(bundle) = bundle else { continue };

            // Update peer display name from relay bundle (no /api/config path for relay-only peers)
            if let Some(ref new_name) = bundle.library_name
                && !new_name.is_empty()
                && *new_name != peer_model.name
                && let Ok(Some(p)) = peer::Entity::find_by_id(peer_model.id).one(db).await
            {
                let mut active: peer::ActiveModel = p.into();
                active.name = Set(new_name.clone());
                active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                let _ = active.update(db).await;
            }

            if !bundle.share_gamification_stats {
                // Peer explicitly does not share - clear cached stats
                let _ = peer_gamification_stats::Entity::delete_many()
                    .filter(peer_gamification_stats::Column::PeerId.eq(peer_model.id))
                    .exec(db)
                    .await;
                continue;
            }

            let Some(stats) = bundle.gamification else {
                continue;
            };

            // Upsert gamification stats directly from bundle (no extra HTTP call needed)
            let _ = peer_gamification_stats::Entity::delete_many()
                .filter(peer_gamification_stats::Column::PeerId.eq(peer_model.id))
                .exec(db)
                .await;

            let entry = peer_gamification_stats::ActiveModel {
                peer_id: Set(peer_model.id),
                library_name: Set(stats.library_name),
                collector_level: Set(stats.collector.level),
                collector_current: Set(stats.collector.current as i32),
                reader_level: Set(stats.reader.level),
                reader_current: Set(stats.reader.current as i32),
                lender_level: Set(stats.lender.level),
                lender_current: Set(stats.lender.current as i32),
                cataloguer_level: Set(stats.cataloguer.level),
                cataloguer_current: Set(stats.cataloguer.current as i32),
                synced_at: Set(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            };

            if let Err(e) = peer_gamification_stats::Entity::insert(entry)
                .exec(db)
                .await
            {
                tracing::warn!(
                    "Leaderboard relay: failed to save gamification stats for peer {}: {}",
                    peer_model.id,
                    e
                );
            } else {
                tracing::info!(
                    "Leaderboard relay: gamification stats synced for peer {} via relay",
                    peer_model.id
                );
            }
        }
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
