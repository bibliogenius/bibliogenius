//! Device pairing and linked device management endpoints.
//!
//! These endpoints replace the legacy pairing code/verify in auth.rs
//! with a proper service-backed implementation that exchanges crypto keys
//! and persists linked devices.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;

use crate::infrastructure::AppState;
use crate::services::device_pairing_service::PairingAcceptInput;
use crate::services::device_sync_service::RemoteOp;

#[derive(Deserialize, Default)]
pub struct TriggerSyncInput {
    /// LAN URL of the peer (from mDNS discovery). Takes priority over stored relay_url.
    pub peer_url: Option<String>,
}

#[derive(Deserialize)]
pub struct GenerateOfferInput {
    pub device_name: String,
    pub library_uuid: String,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
    pub relay_write_token: Option<String>,
}

/// POST /api/devices/pair/offer - Generate a 6-digit pairing offer
pub async fn generate_offer(
    State(state): State<AppState>,
    Json(input): Json<GenerateOfferInput>,
) -> impl IntoResponse {
    match state.device_pairing.generate_offer(
        input.device_name,
        input.library_uuid,
        input.relay_url,
        input.mailbox_id,
        input.relay_write_token,
    ) {
        Ok(resp) => (StatusCode::OK, Json(json!(resp))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// POST /api/devices/pair/accept - Accept a pairing offer and register the device
pub async fn accept_offer(
    State(state): State<AppState>,
    Json(input): Json<PairingAcceptInput>,
) -> impl IntoResponse {
    match state.device_pairing.accept_offer(input).await {
        Ok(confirmation) => (StatusCode::OK, Json(json!(confirmation))).into_response(),
        Err(e) => {
            let status = if e.contains("expired") || e.contains("Invalid") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({"error": e}))).into_response()
        }
    }
}

/// POST /api/devices/register - Register a linked device directly (used by acceptor after pairing)
pub async fn register_device(
    State(state): State<AppState>,
    Json(input): Json<RegisterDeviceInput>,
) -> impl IntoResponse {
    use crate::domain::CreateLinkedDeviceInput;
    match state
        .linked_device_repo
        .create(CreateLinkedDeviceInput {
            name: input.name,
            ed25519_public_key: input.ed25519_public_key,
            x25519_public_key: input.x25519_public_key,
            relay_url: input.relay_url,
            mailbox_id: input.mailbox_id,
            relay_write_token: None,
        })
        .await
    {
        Ok(device) => (StatusCode::CREATED, Json(json!({"device_id": device.id}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct RegisterDeviceInput {
    pub name: String,
    pub ed25519_public_key: Vec<u8>,
    pub x25519_public_key: Vec<u8>,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
}

/// GET /api/devices - List all linked devices
pub async fn list_devices(State(state): State<AppState>) -> impl IntoResponse {
    match state.device_pairing.list_devices().await {
        Ok(devices) => (StatusCode::OK, Json(json!(devices))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// DELETE /api/devices/:id - Remove a linked device
pub async fn remove_device(
    State(state): State<AppState>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    match state.device_pairing.remove_device(id).await {
        Ok(()) => (StatusCode::OK, Json(json!({"message": "Device removed"}))).into_response(),
        Err(e) => {
            let status = match e {
                crate::domain::DomainError::NotFound => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({"error": e.to_string()}))).into_response()
        }
    }
}

// ── Sync control endpoints ───────────────────────────────────────────

/// POST /api/devices/sync/:id - Trigger sync with a specific linked device (LAN only)
pub async fn trigger_sync(
    State(state): State<AppState>,
    Path(device_id): Path<i32>,
    body: Option<Json<TriggerSyncInput>>,
) -> impl IntoResponse {
    let input = body.map(|b| b.0).unwrap_or_default();
    tracing::debug!(
        "DeviceSync: trigger_sync device_id={device_id} peer_url={:?}",
        input.peer_url
    );
    // 1. Look up the device
    let device = match state.linked_device_repo.find_by_id(device_id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Device not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    // 2. We need the crypto service + DirectTransport for E2EE
    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "E2EE not initialized"})),
            )
                .into_response();
        }
    };

    // 3. Build the X25519 public key from the device's stored key
    let x25519_bytes: [u8; 32] = match device.x25519_public_key.as_slice().try_into() {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid device X25519 key"})),
            )
                .into_response();
        }
    };
    let peer_x25519 = x25519_dalek::PublicKey::from(x25519_bytes);

    // Build PeerInfo for E2EE
    let ed25519_bytes: [u8; 32] = match device.ed25519_public_key.as_slice().try_into() {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid device Ed25519 key"})),
            )
                .into_response();
        }
    };
    let verifying_key = match ed25519_dalek::VerifyingKey::from_bytes(&ed25519_bytes) {
        Ok(k) => k,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid Ed25519 verifying key"})),
            )
                .into_response();
        }
    };

    let peer_info = crate::services::crypto_service::PeerInfo {
        verifying_key,
        x25519_public: peer_x25519,
    };

    // 4. Collect local ops since the device's last sync
    let since = device.last_synced.as_deref();
    tracing::debug!("DeviceSync: collecting local ops since={since:?}");
    let local_ops = state
        .device_sync
        .get_local_ops_since(since)
        .await
        .unwrap_or_default();
    tracing::debug!("DeviceSync: collected {} local ops", local_ops.len());

    let ops_payload: Vec<serde_json::Value> = local_ops
        .iter()
        .map(|op| {
            json!({
                "entity_type": op.entity_type,
                "entity_id": op.entity_id,
                "operation": op.operation,
                "payload": op.payload.as_ref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
                "created_at": op.created_at,
            })
        })
        .collect();

    // 5. Build sync request message
    let transport = crate::services::e2ee_transport::DirectTransport::new(crypto_service);
    let message = crate::services::e2ee_transport::DirectTransport::build_message(
        "device_sync_request",
        json!({
            "since": since,
            "device_id": device_id,
            "ops": ops_payload,
        }),
    );

    // 6. Use LAN URL from request body (mDNS), falling back to stored relay_url.
    let peer_url = input
        .peer_url
        .filter(|u| !u.is_empty())
        .or_else(|| device.relay_url.clone().filter(|u| !u.is_empty()));

    let peer_url = match peer_url {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "No reachable URL for device (no LAN peer and no relay URL configured)"})),
            )
                .into_response();
        }
    };

    // 7. Send and process response
    println!(
        "DeviceSync: sending {} ops to {peer_url}",
        ops_payload.len()
    );
    match transport
        .send(&peer_url, &peer_x25519, &peer_info, &message)
        .await
    {
        Ok(Some(response)) => {
            println!(
                "DeviceSync: got encrypted response, type={}",
                response.message_type
            );
            // Process response ops
            let response_ops: Vec<RemoteOp> = response
                .payload
                .get("ops")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            let safety_mode = {
                use crate::models::installation_profile::ProfileConfig;
                match ProfileConfig::load(state.db()).await {
                    Ok(config) => config.is_module_enabled("sync_safety"),
                    Err(_) => true,
                }
            };

            let received_count = if !response_ops.is_empty() {
                match state
                    .device_sync
                    .receive_remote_ops(device_id, response_ops, safety_mode)
                    .await
                {
                    Ok(result) => result.inserted_count,
                    Err(e) => {
                        tracing::error!("Sync: Failed to process response ops: {e}");
                        0
                    }
                }
            } else {
                0
            };

            // Update last_synced
            let _ = state
                .device_sync
                .update_device_last_synced(device_id, &chrono::Utc::now().to_rfc3339())
                .await;

            let pending_review_count = state
                .device_sync
                .get_pending_review_ops()
                .await
                .map(|ops| ops.len() as u32)
                .unwrap_or(0);

            (
                StatusCode::OK,
                Json(json!({
                    "sent_count": ops_payload.len(),
                    "received_count": received_count,
                    "pending_review_count": pending_review_count,
                })),
            )
                .into_response()
        }
        Ok(None) => {
            tracing::debug!("DeviceSync: peer returned no encrypted response (fire-and-forget)");
            // Peer returned no encrypted response (fire-and-forget style)
            let _ = state
                .device_sync
                .update_device_last_synced(device_id, &chrono::Utc::now().to_rfc3339())
                .await;

            (
                StatusCode::OK,
                Json(json!({
                    "sent_count": ops_payload.len(),
                    "received_count": 0,
                    "pending_review_count": 0,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::debug!("DeviceSync: transport error: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("Sync failed: {e}")})),
            )
                .into_response()
        }
    }
}

/// GET /api/devices/sync/pending-review - List operations pending review
pub async fn sync_pending_review(State(state): State<AppState>) -> impl IntoResponse {
    match state.device_sync.get_pending_review_ops().await {
        Ok(ops) => {
            let payload: Vec<serde_json::Value> = ops
                .iter()
                .map(|op| {
                    json!({
                        "id": op.id,
                        "entity_type": op.entity_type,
                        "entity_id": op.entity_id,
                        "operation": op.operation,
                        "payload": op.payload,
                        "source": op.source,
                        "created_at": op.created_at,
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!(payload))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct SyncApproveRejectInput {
    pub ids: Option<Vec<i32>>,
    pub all: Option<bool>,
}

/// POST /api/devices/sync/approve - Approve pending review operations
pub async fn sync_approve(
    State(state): State<AppState>,
    Json(input): Json<SyncApproveRejectInput>,
) -> impl IntoResponse {
    let count = if input.all.unwrap_or(false) {
        state.device_sync.approve_all_pending_review().await
    } else if let Some(ids) = &input.ids {
        state.device_sync.approve_ops(ids).await
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Provide 'ids' or 'all: true'"})),
        )
            .into_response();
    };

    match count {
        Ok(n) => (StatusCode::OK, Json(json!({"approved": n}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/devices/sync/reject - Reject pending review operations
pub async fn sync_reject(
    State(state): State<AppState>,
    Json(input): Json<SyncApproveRejectInput>,
) -> impl IntoResponse {
    let count = if input.all.unwrap_or(false) {
        state.device_sync.reject_all_pending_review().await
    } else if let Some(ids) = &input.ids {
        state.device_sync.reject_ops(ids).await
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Provide 'ids' or 'all: true'"})),
        )
            .into_response();
    };

    match count {
        Ok(n) => (StatusCode::OK, Json(json!({"rejected": n}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
