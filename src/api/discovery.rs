//! Discovery API Endpoints
//!
//! Provides REST endpoints for local network discovery via mDNS.

use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;

use crate::services::{get_local_peers, is_mdns_active};

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
            "service_type": "_bibliogenius._tcp.local."
        })),
    )
}

#[derive(Deserialize)]
pub struct ToggleRequest {
    pub enabled: bool,
}

/// POST /api/discovery/toggle
/// Enable or disable mDNS discovery (placeholder - requires app restart for now)
pub async fn toggle_mdns(Json(payload): Json<ToggleRequest>) -> impl IntoResponse {
    // Note: Full dynamic toggle requires more complex state management.
    // For now, we return the desired state and note that restart is needed.
    (
        StatusCode::OK,
        Json(json!({
            "message": if payload.enabled {
                "mDNS is enabled at startup. Restart the server to apply changes."
            } else {
                "To disable mDNS, set MDNS_ENABLED=false and restart the server."
            },
            "requested": payload.enabled,
            "current": is_mdns_active()
        })),
    )
}
