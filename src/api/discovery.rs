//! Discovery API Endpoints
//!
//! Provides REST endpoints for local network discovery via mDNS.

use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;

use crate::services::{
    MAX_DISCOVERED_PEERS, get_local_peer_count, get_local_peers, is_mdns_active, restart_mdns,
    stop_mdns,
};

/// GET /api/discovery/local
/// Returns list of discovered BiblioGenius libraries on the local network
pub async fn list_local_peers() -> impl IntoResponse {
    let peers = get_local_peers();
    (
        StatusCode::OK,
        Json(json!({
            "peers": peers,
            "count": peers.len(),
            "mdns_active": is_mdns_active()
        })),
    )
}

/// GET /api/discovery/status
/// Returns the current status of mDNS service
pub async fn mdns_status() -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "active": is_mdns_active(),
            "service_type": "_bibliogenius._tcp.local.",
            "peer_count": get_local_peer_count(),
            "max_peers": MAX_DISCOVERED_PEERS
        })),
    )
}

#[derive(Deserialize)]
pub struct ToggleRequest {
    pub enabled: bool,
}

/// POST /api/discovery/toggle
/// Enable or disable mDNS discovery at runtime
pub async fn toggle_mdns(Json(payload): Json<ToggleRequest>) -> impl IntoResponse {
    if payload.enabled {
        // Start/restart mDNS using stored config from init_mdns
        match restart_mdns() {
            Ok(()) => (
                StatusCode::OK,
                Json(json!({
                    "message": "mDNS enabled",
                    "active": true
                })),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": e,
                    "active": is_mdns_active()
                })),
            ),
        }
    } else {
        stop_mdns();
        (
            StatusCode::OK,
            Json(json!({
                "message": "mDNS disabled",
                "active": false
            })),
        )
    }
}
