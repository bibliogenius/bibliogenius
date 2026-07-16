//! Relay setup and configuration endpoints, relay library sync (ADR-012).

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct SetupRelayRequest {
    pub relay_url: String,
}

/// POST /api/peers/relay/setup — Register a mailbox on a relay hub.
///
/// Calls the relay hub to create a new mailbox, then stores the config locally.
pub async fn setup_relay(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<SetupRelayRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Validate relay URL
    if let Err(e) = validate_url(&payload.relay_url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Call relay hub to create a mailbox
    let client = get_safe_client();
    let url = format!(
        "{}/api/relay/mailbox",
        payload.relay_url.trim_end_matches('/')
    );

    let response = match client.post(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Failed to reach relay hub: {e}") })),
            )
                .into_response();
        }
    };

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Relay hub returned error: {body}") })),
        )
            .into_response();
    }

    let result: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Invalid relay response: {e}") })),
            )
                .into_response();
        }
    };

    let mailbox_uuid = result
        .get("uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let read_token = result
        .get("read_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let write_token = result
        .get("write_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if mailbox_uuid.is_empty() || read_token.is_empty() || write_token.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Relay hub returned incomplete mailbox data" })),
        )
            .into_response();
    }

    // 3. Persist the new mailbox and conditionally invalidate the hub
    //    directory config. Same-URL re-setups must preserve the write_token,
    //    otherwise the next heartbeat loops on 401 against the existing hub
    //    profile that only the purged token could authenticate.
    let relay_url_for_notify = payload.relay_url.clone();

    let hub_changed = match apply_relay_setup(
        db,
        &payload.relay_url,
        &mailbox_uuid,
        &read_token,
        &write_token,
    )
    .await
    {
        Ok(changed) => changed,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to save relay config: {e}") })),
            )
                .into_response();
        }
    };

    tracing::info!("Relay: Mailbox registered");

    // Keep HUB_URL in sync so hub_directory_service uses the same hub.
    // SAFETY: single-threaded write path (same pattern as set_hub_url_ffi).
    unsafe { std::env::set_var("HUB_URL", &relay_url_for_notify) };

    if hub_changed {
        tracing::info!(
            "Relay: HUB_URL updated to {}, directory config invalidated (hub changed)",
            &relay_url_for_notify
        );
    } else {
        tracing::info!(
            "Relay: HUB_URL set to {}, directory config preserved (hub unchanged)",
            &relay_url_for_notify
        );
    }

    // Proactively notify all E2EE peers of the new mailbox credentials.
    // This prevents the window where peers have stale relay info after a hub switch.
    let state_clone = state.clone();
    let mailbox_uuid_for_notify = mailbox_uuid.clone();
    tokio::spawn(async move {
        crate::services::relay_poller::notify_peers_of_new_credentials(
            &state_clone,
            &relay_url_for_notify,
            &mailbox_uuid_for_notify,
        )
        .await;
    });

    (
        StatusCode::OK,
        Json(json!({
            "mailbox_uuid": mailbox_uuid,
            "write_token": write_token,
        })),
    )
        .into_response()
}

/// Persist a freshly-registered relay mailbox and invalidate the hub
/// directory config only when the hub URL actually changes.
///
/// Extracted from `setup_relay` so the DB-level conditional can be tested
/// without standing up a mock relay server. Returns `true` if the hub URL
/// differed from the previous config (and `hub_directory_config` was
/// therefore wiped), `false` otherwise.
async fn apply_relay_setup(
    db: &DatabaseConnection,
    relay_url: &str,
    mailbox_uuid: &str,
    read_token: &str,
    write_token: &str,
) -> Result<bool, sea_orm::DbErr> {
    use crate::models::relay_config;
    use sea_orm::ConnectionTrait;

    let previous_hub_url: Option<String> = relay_config::Entity::find()
        .one(db)
        .await?
        .map(|m| m.relay_url);

    db.execute(sea_orm::Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM my_relay_config".to_owned(),
    ))
    .await?;

    let now = chrono::Utc::now().to_rfc3339();
    relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(relay_url.to_string()),
        mailbox_uuid: Set(mailbox_uuid.to_string()),
        read_token: Set(read_token.to_string()),
        write_token: Set(write_token.to_string()),
        created_at: Set(now),
    }
    .insert(db)
    .await?;

    crate::services::relay_session::mark_mailbox_created_this_session();

    let hub_changed = previous_hub_url
        .as_deref()
        .is_some_and(|prev| crate::utils::hub_url::hub_urls_differ(prev, relay_url));

    if hub_changed {
        db.execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM hub_directory_config".to_owned(),
        ))
        .await?;
    }

    Ok(hub_changed)
}

/// GET /api/peers/relay/config — Get current relay config (if any).
pub async fn get_relay_config_endpoint(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    match crate::api::relay::get_my_relay_config(db).await {
        Some(config) => (
            StatusCode::OK,
            Json(json!({
                "relay_url": config.relay_url,
                "mailbox_uuid": config.mailbox_uuid,
                "write_token": config.write_token,
                "created_at": config.created_at,
            })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No relay configured" })),
        )
            .into_response(),
    }
}

/// DELETE /api/peers/relay/config - Remove relay config (disconnect from hub).
///
/// Before deleting the local config, attempts to delete the mailbox on the hub
/// so it does not linger as an orphan accepting stale deposits.
pub async fn delete_relay_config_endpoint(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Read current config before deleting (need mailbox UUID + read_token for hub cleanup)
    let config = crate::api::relay::get_my_relay_config(db).await;

    // 2. Best-effort: delete the mailbox on the hub
    if let Some(ref cfg) = config {
        let url = format!("{}/api/relay/mailbox/{}", cfg.relay_url, cfg.mailbox_uuid);
        let client = get_safe_client();
        match client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", cfg.read_token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("Relay: Deleted mailbox on hub");
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "Relay: Hub mailbox delete returned {} body={}",
                    status,
                    body
                );
            }
            Err(e) => {
                tracing::warn!("Relay: Failed to delete mailbox on hub: {e}");
            }
        }
    }

    // 3. Delete local config
    use sea_orm::ConnectionTrait;
    match db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM my_relay_config".to_owned(),
        ))
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Relay config removed" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to remove relay config: {e}") })),
        )
            .into_response(),
    }
}

/// Send a library sync request to a peer via E2EE (relay or direct).
/// Returns the response payload if available, or starts async relay flow.
///
/// POST /api/peers/relay/library_request
/// Body: { "peer_id": int, "request_type": "manifest"|"page"|"search", ... }
#[derive(Deserialize)]
pub struct RelayLibraryRequest {
    pub peer_id: i32,
    pub request_type: String,
    #[serde(default)]
    pub cursor: Option<i64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub query: Option<String>,
}

pub async fn relay_library_request(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<RelayLibraryRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find the peer
    let the_peer = match peer::Entity::find_by_id(req.peer_id).one(db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 2. Build the E2EE message type and payload
    let (message_type, payload) = match req.request_type.as_str() {
        "manifest" => ("library_manifest_request", json!({})),
        "page" => (
            "library_page_request",
            json!({
                "cursor": req.cursor,
                "limit": req.limit.unwrap_or(50),
            }),
        ),
        "search" => (
            "library_search_request",
            json!({
                "query": req.query.unwrap_or_default(),
                "limit": req.limit.unwrap_or(20),
            }),
        ),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid request_type. Use: manifest, page, search" })),
            )
                .into_response();
        }
    };

    // 3. Send via E2EE (direct or relay with reply-to)
    tracing::info!(
        "Relay library request: type='{}' peer='{}' (id={})",
        req.request_type,
        the_peer.name,
        the_peer.id,
    );

    match try_send_e2ee(&state, &the_peer, message_type, payload).await {
        Ok(Some(Some(response))) => {
            // Direct response (LAN path)
            tracing::info!(
                "Relay library request: '{}' for peer '{}' resolved via direct LAN",
                req.request_type,
                the_peer.name
            );
            (StatusCode::OK, Json(response.payload)).into_response()
        }
        Ok(Some(None)) => {
            // Sent via relay (no immediate response)
            tracing::info!(
                "Relay library request: '{}' for peer '{}' sent via relay (pending)",
                req.request_type,
                the_peer.name
            );
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "relay_pending",
                    "message": "Request sent via relay. Use poll_now to check for response.",
                })),
            )
                .into_response()
        }
        Ok(None) => {
            // E2EE not available - no plaintext fallback for library sync
            tracing::warn!(
                "Relay library request: '{}' for peer '{}' failed - E2EE not available",
                req.request_type,
                the_peer.name
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "E2EE not available for this peer" })),
            )
                .into_response()
        }
        Err(e)
            if e.contains("peer unreachable for credential refresh")
                || e.contains("failed after credential refresh") =>
        {
            // Peer's mailbox expired and we cannot refresh credentials.
            // Return 502 so the client stops retrying (circuit breaker).
            tracing::warn!(
                "Relay library request: '{}' for peer '{}' - peer unreachable (502): {}",
                req.request_type,
                the_peer.name,
                e
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "peer_unreachable",
                    "message": e,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(
                "Relay library request: '{}' for peer '{}' failed (500): {}",
                req.request_type,
                the_peer.name,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    }
}

/// Wait for a pending relay response by correlation_id.
///
/// POST /api/peers/relay/await_response
/// Body: { "correlation_id": "uuid", "timeout_ms": 5000 }
#[derive(Deserialize)]
pub struct AwaitRelayResponse {
    pub correlation_id: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

pub async fn await_relay_response(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<AwaitRelayResponse>,
) -> impl IntoResponse {
    let timeout = std::time::Duration::from_millis(req.timeout_ms.min(30_000));

    // Register a new listener (or check if one already exists)
    let rx = state.register_relay_request(req.correlation_id.clone());

    // Wait for the response with timeout
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(payload)) => (StatusCode::OK, Json(payload)).into_response(),
        Ok(Err(_)) => {
            // Sender dropped (cancelled)
            (
                StatusCode::GONE,
                Json(json!({ "error": "Request was cancelled" })),
            )
                .into_response()
        }
        Err(_) => {
            // Timeout - clean up
            state.cancel_relay_request(&req.correlation_id);
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(json!({ "status": "timeout", "message": "No response yet" })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod relay_setup_tests {
    use super::*;
    use crate::db;
    use crate::services::relay_session;
    use sea_orm::{ConnectionTrait, Statement};
    use serial_test::serial;

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn seed_directory_config(db: &DatabaseConnection, token: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            format!(
                "INSERT INTO hub_directory_config
                     (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, recovery_code, created_at, updated_at)
                 VALUES (1, 'test-node', '{token}', 0, 1, 'everyone', 1, 'rc-1', '{now}', '{now}')"
            ),
        ))
        .await
        .unwrap();
    }

    async fn directory_token(db: &DatabaseConnection) -> Option<String> {
        db.query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT write_token FROM hub_directory_config WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .and_then(|row| row.try_get::<String>("", "write_token").ok())
    }

    /// Re-registering a mailbox against the **same** hub must keep the
    /// write_token in hub_directory_config intact. Before the fix, the
    /// unconditional DELETE wiped the token on every setup, causing the
    /// next profile heartbeat to hit 401 in a loop (stuck Eve scenario).
    #[tokio::test]
    #[serial]
    async fn apply_relay_setup_preserves_directory_config_when_hub_unchanged() {
        relay_session::reset_for_tests();
        let db = setup_db().await;

        apply_relay_setup(&db, "https://hub.example.org", "mbx-1", "rtok-1", "wtok-1")
            .await
            .expect("first setup");

        seed_directory_config(&db, "preserved-token").await;

        let changed =
            apply_relay_setup(&db, "https://hub.example.org/", "mbx-2", "rtok-2", "wtok-2")
                .await
                .expect("second setup same hub");

        assert!(!changed, "hub URL should be detected as unchanged");
        assert_eq!(
            directory_token(&db).await.as_deref(),
            Some("preserved-token"),
            "hub_directory_config must survive a same-URL re-setup",
        );
        assert!(
            relay_session::mailbox_created_this_session(),
            "apply_relay_setup must mark the session flag",
        );
    }

    /// A genuine hub swap still invalidates the directory config, since the
    /// write_token from the old hub cannot authenticate against the new one.
    #[tokio::test]
    #[serial]
    async fn apply_relay_setup_wipes_directory_config_when_hub_changes() {
        relay_session::reset_for_tests();
        let db = setup_db().await;

        apply_relay_setup(
            &db,
            "https://hub-a.example.org",
            "mbx-1",
            "rtok-1",
            "wtok-1",
        )
        .await
        .expect("first setup");

        seed_directory_config(&db, "stale-token").await;

        let changed = apply_relay_setup(
            &db,
            "https://hub-b.example.org",
            "mbx-2",
            "rtok-2",
            "wtok-2",
        )
        .await
        .expect("second setup new hub");

        assert!(changed, "hub URL change must be detected");
        assert!(
            directory_token(&db).await.is_none(),
            "hub_directory_config must be wiped when the hub actually changes",
        );
        assert!(
            relay_session::mailbox_created_this_session(),
            "apply_relay_setup must mark the session flag",
        );
    }

    /// First-time setup (no previous relay config) is neither a "same hub"
    /// nor a "hub change" — we simply have nothing to invalidate.
    #[tokio::test]
    #[serial]
    async fn apply_relay_setup_first_time_reports_no_change() {
        relay_session::reset_for_tests();
        let db = setup_db().await;

        assert!(
            !relay_session::mailbox_created_this_session(),
            "flag must start unset",
        );

        let changed =
            apply_relay_setup(&db, "https://hub.example.org", "mbx-1", "rtok-1", "wtok-1")
                .await
                .expect("first setup");

        assert!(!changed, "no previous hub means no change to signal");
        assert!(
            relay_session::mailbox_created_this_session(),
            "apply_relay_setup must mark the session flag",
        );
    }
}
