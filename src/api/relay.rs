//! Relay hub endpoints — blind mailbox for E2EE WAN message delivery.
//!
//! Any BiblioGenius instance can serve as a relay hub. The relay stores
//! opaque encrypted blobs without being able to read them.
//!
//! See SECURITY_GUIDELINES.md §B8 for token auth model.

use axum::{
    body::Bytes,
    extract::{Json, Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set, Statement,
};
use serde_json::json;

use crate::models::relay_config;

/// Maximum blob size: 64 KB
const MAX_BLOB_SIZE: usize = 64 * 1024;
/// Maximum messages per mailbox
const MAX_MESSAGES_PER_MAILBOX: u64 = 100;
/// Message TTL: 30 days
const MESSAGE_TTL_DAYS: i64 = 30;
/// Mailbox inactivity TTL: 90 days
const MAILBOX_TTL_DAYS: i64 = 90;

// ── Relay mailbox models (used as SeaORM entities for hub storage) ────

mod relay_mailbox {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "relay_mailboxes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub uuid: String,
        pub read_token: String,
        pub write_token: String,
        pub created_at: String,
        pub last_accessed: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod relay_message {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "relay_messages")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub mailbox_uuid: String,
        pub blob: Vec<u8>,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Generate a 256-bit random token, base64url-encoded (per B8).
fn generate_token() -> String {
    use base64::Engine;
    use rand::RngCore;

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Extract bearer token from Authorization header.
fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Probabilistic TTL cleanup (~1% chance per call).
/// Deletes messages older than 30 days and mailboxes inactive for 90 days.
async fn maybe_cleanup(db: &sea_orm::DatabaseConnection) {
    use rand::Rng;
    if rand::thread_rng().gen_range(0..100) != 0 {
        return;
    }

    tracing::info!("Relay: Running probabilistic TTL cleanup");

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            format!(
                "DELETE FROM relay_messages WHERE created_at < datetime('now', '-{MESSAGE_TTL_DAYS} days')"
            ),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            format!(
                "DELETE FROM relay_mailboxes WHERE last_accessed IS NOT NULL AND last_accessed < datetime('now', '-{MAILBOX_TTL_DAYS} days')"
            ),
        ))
        .await;

    // Also clean up mailboxes that have never been accessed and are old
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            format!(
                "DELETE FROM relay_mailboxes WHERE last_accessed IS NULL AND created_at < datetime('now', '-{MAILBOX_TTL_DAYS} days')"
            ),
        ))
        .await;
}

// ── Endpoints ────────────────────────────────────────────────────────

/// POST /api/relay/mailbox — Create a new mailbox.
/// No authentication required (anyone can create a mailbox).
/// Returns { uuid, read_token, write_token }.
pub async fn create_mailbox(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    let uuid = uuid::Uuid::new_v4().to_string();
    let read_token = generate_token();
    let write_token = generate_token();
    let now = chrono::Utc::now().to_rfc3339();

    let mailbox = relay_mailbox::ActiveModel {
        uuid: Set(uuid.clone()),
        read_token: Set(read_token.clone()),
        write_token: Set(write_token.clone()),
        created_at: Set(now),
        last_accessed: Set(None),
    };

    match mailbox.insert(db).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(json!({
                "uuid": uuid,
                "read_token": read_token,
                "write_token": write_token,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Relay: Failed to create mailbox: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to create mailbox" })),
            )
                .into_response()
        }
    }
}

/// POST /api/relay/mailbox/:uuid/messages — Deposit an encrypted blob.
/// Requires: Authorization: Bearer {write_token}
pub async fn deposit_message(
    State(state): State<crate::infrastructure::AppState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Extract and validate write token
    let token = match extract_bearer_token(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing Authorization header" })),
            )
                .into_response();
        }
    };

    // 2. Find mailbox and verify write token
    let mailbox = match relay_mailbox::Entity::find_by_id(&uuid).one(db).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Mailbox not found" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("Relay: DB error looking up mailbox: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Database error" })),
            )
                .into_response();
        }
    };

    if mailbox.write_token != token {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid write token" })),
        )
            .into_response();
    }

    // 3. Check blob size
    if body.len() > MAX_BLOB_SIZE {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(
                json!({ "error": format!("Blob exceeds maximum size of {} bytes", MAX_BLOB_SIZE) }),
            ),
        )
            .into_response();
    }

    // 4. Check message count limit
    let count = relay_message::Entity::find()
        .filter(relay_message::Column::MailboxUuid.eq(&uuid))
        .count(db)
        .await
        .unwrap_or(0);

    if count >= MAX_MESSAGES_PER_MAILBOX {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": format!("Mailbox full ({MAX_MESSAGES_PER_MAILBOX} messages max)") })),
        )
            .into_response();
    }

    // 5. Store blob
    let now = chrono::Utc::now().to_rfc3339();
    let msg = relay_message::ActiveModel {
        mailbox_uuid: Set(uuid),
        blob: Set(body.to_vec()),
        created_at: Set(now),
        ..Default::default()
    };

    match msg.insert(db).await {
        Ok(inserted) => (StatusCode::CREATED, Json(json!({ "id": inserted.id }))).into_response(),
        Err(e) => {
            tracing::error!("Relay: Failed to deposit message: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to store message" })),
            )
                .into_response()
        }
    }
}

/// GET /api/relay/mailbox/:uuid/messages — Collect pending messages.
/// Requires: Authorization: Bearer {read_token}
pub async fn collect_messages(
    State(state): State<crate::infrastructure::AppState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Extract and validate read token
    let token = match extract_bearer_token(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing Authorization header" })),
            )
                .into_response();
        }
    };

    // 2. Find mailbox and verify read token
    let mailbox = match relay_mailbox::Entity::find_by_id(&uuid).one(db).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Mailbox not found" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("Relay: DB error looking up mailbox: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Database error" })),
            )
                .into_response();
        }
    };

    if mailbox.read_token != token {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid read token" })),
        )
            .into_response();
    }

    // 3. Update last_accessed
    let mut active: relay_mailbox::ActiveModel = mailbox.into();
    active.last_accessed = Set(Some(chrono::Utc::now().to_rfc3339()));
    let _ = active.update(db).await;

    // 4. Fetch all pending messages
    let messages = relay_message::Entity::find()
        .filter(relay_message::Column::MailboxUuid.eq(&uuid))
        .order_by_asc(relay_message::Column::Id)
        .all(db)
        .await
        .unwrap_or_default();

    use base64::Engine;
    let items: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id,
                "blob": base64::engine::general_purpose::STANDARD.encode(&m.blob),
                "created_at": m.created_at,
            })
        })
        .collect();

    // 5. Probabilistic cleanup
    maybe_cleanup(db).await;

    (StatusCode::OK, Json(json!({ "messages": items }))).into_response()
}

/// DELETE /api/relay/mailbox/:uuid/messages/:id — Acknowledge and delete a message.
/// Requires: Authorization: Bearer {read_token}
pub async fn ack_message(
    State(state): State<crate::infrastructure::AppState>,
    Path((uuid, message_id)): Path<(String, i32)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Extract and validate read token
    let token = match extract_bearer_token(&headers) {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing Authorization header" })),
            )
                .into_response();
        }
    };

    // 2. Find mailbox and verify read token
    let mailbox = match relay_mailbox::Entity::find_by_id(&uuid).one(db).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Mailbox not found" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("Relay: DB error looking up mailbox: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Database error" })),
            )
                .into_response();
        }
    };

    if mailbox.read_token != token {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid read token" })),
        )
            .into_response();
    }

    // 3. Delete the message (only if it belongs to this mailbox)
    let result = relay_message::Entity::find_by_id(message_id)
        .filter(relay_message::Column::MailboxUuid.eq(&uuid))
        .one(db)
        .await;

    match result {
        Ok(Some(msg)) => {
            let active: relay_message::ActiveModel = msg.into();
            match active.delete(db).await {
                Ok(_) => (StatusCode::OK, Json(json!({ "message": "Deleted" }))).into_response(),
                Err(e) => {
                    tracing::error!("Relay: Failed to delete message: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": "Failed to delete message" })),
                    )
                        .into_response()
                }
            }
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Message not found" })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Relay: DB error looking up message: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Database error" })),
            )
                .into_response()
        }
    }
}

// ── Adaptive polling (ADR-012) ─────────────────────────────────────

/// POST /api/relay/poll_now - Trigger an immediate relay poll cycle.
///
/// Used by Flutter when awaiting a relay response to reduce latency
/// from ~120s (background polling) to ~10-15s (adaptive fast-polling).
pub async fn poll_now(State(state): State<crate::infrastructure::AppState>) -> impl IntoResponse {
    match crate::services::relay_poller::poll_once(&state).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "message": "Poll completed" }))).into_response(),
        Err(e) => {
            tracing::warn!("poll_now: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    }
}

// ── Client-side helpers (used by peer.rs setup_relay) ────────────────

/// Get the local relay config (singleton row from my_relay_config).
pub async fn get_my_relay_config(db: &sea_orm::DatabaseConnection) -> Option<relay_config::Model> {
    relay_config::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten()
}
