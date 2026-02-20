//! E2EE message endpoint — single encrypted entry point for all peer-to-peer messages.
//!
//! `POST /api/e2ee/message` receives an `EncryptedEnvelope`, opens it,
//! and dispatches by `message_type` to existing business logic.

use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::json;

use crate::crypto::envelope::{ClearMessage, EncryptedEnvelope};
use crate::infrastructure::AppState;
use crate::models::peer;
use crate::services::crypto_service::PeerInfo;

/// Receive and process an encrypted peer message.
///
/// Pipeline: open envelope → identify sender → dispatch by message_type → optional encrypted response.
pub async fn receive_encrypted_message(
    State(state): State<AppState>,
    Json(envelope): Json<EncryptedEnvelope>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Get crypto service
    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "E2EE not initialized" })),
            )
                .into_response();
        }
    };

    // 2. Load all peers with key_exchange_done
    let peers = match peer::Entity::find()
        .filter(peer::Column::KeyExchangeDone.eq(true))
        .all(db)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("E2EE: Failed to load peers: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to load peers" })),
            )
                .into_response();
        }
    };

    // 3. Build PeerInfo vec from peers with valid keys
    let mut known_peers: Vec<PeerInfo> = Vec::new();
    let mut peer_models: Vec<&peer::Model> = Vec::new();

    for p in &peers {
        if let (Some(ed_hex), Some(x_hex)) = (&p.public_key, &p.x25519_public_key)
            && let (Ok(ed_bytes), Ok(x_bytes)) = (hex::decode(ed_hex), hex::decode(x_hex))
            && ed_bytes.len() == 32
            && x_bytes.len() == 32
        {
            let ed_arr: [u8; 32] = ed_bytes.try_into().unwrap();
            let x_arr: [u8; 32] = x_bytes.try_into().unwrap();

            if let Ok(verifying_key) = ed25519_dalek::VerifyingKey::from_bytes(&ed_arr) {
                let x25519_public = x25519_dalek::PublicKey::from(x_arr);
                known_peers.push(PeerInfo {
                    verifying_key,
                    x25519_public,
                });
                peer_models.push(p);
            }
        }
    }

    if known_peers.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "No known E2EE peers" })),
        )
            .into_response();
    }

    // 4. Open the envelope
    let (clear_message, peer_index) = match crypto_service.open(&envelope, &known_peers) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!("E2EE: Failed to open envelope: {e}");
            let status = match e {
                crate::crypto::errors::CryptoError::ReplayDetected => StatusCode::CONFLICT,
                crate::crypto::errors::CryptoError::UnknownSender => StatusCode::FORBIDDEN,
                crate::crypto::errors::CryptoError::MessageExpired => StatusCode::GONE,
                _ => StatusCode::BAD_REQUEST,
            };
            return (status, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    let sender_peer = peer_models[peer_index];
    tracing::info!(
        "E2EE: Received '{}' from peer {} ({})",
        clear_message.message_type,
        sender_peer.name,
        sender_peer.id
    );

    // 5. Dispatch by message_type
    match clear_message.message_type.as_str() {
        "loan_request" => handle_loan_request(db, sender_peer, &clear_message).await,

        "loan_confirmation" => handle_loan_confirmation(db, &clear_message).await,

        "book_sync_request" => {
            let response_payload = handle_book_sync_request(db).await;
            // Seal response back to sender
            seal_response(
                &crypto_service,
                &known_peers[peer_index],
                "book_sync_response",
                response_payload,
            )
        }

        "search_request" => {
            let response_payload = handle_search_request(db, &clear_message).await;
            seal_response(
                &crypto_service,
                &known_peers[peer_index],
                "search_response",
                response_payload,
            )
        }

        "status_update" => handle_status_update(db, &clear_message).await,

        _ => {
            tracing::warn!(
                "E2EE: Unknown message type '{}'",
                clear_message.message_type
            );
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Unknown message type: {}", clear_message.message_type) })),
            )
                .into_response()
        }
    }
}

// ── Dispatch handlers ──────────────────────────────────────────────────

/// Handle an encrypted loan request (same logic as `receive_loan_request` in peer.rs).
async fn handle_loan_request(
    db: &DatabaseConnection,
    sender_peer: &peer::Model,
    msg: &ClearMessage,
) -> axum::response::Response {
    use crate::models::p2p_request;

    let book_isbn = msg
        .payload
        .get("book_isbn")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let book_title = msg
        .payload
        .get("book_title")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if book_isbn.is_empty() && book_title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing book_isbn or book_title" })),
        )
            .into_response();
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let new_request = p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(sender_peer.id),
        book_isbn: Set(book_isbn.to_string()),
        book_title: Set(book_title.to_string()),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    };

    match new_request.insert(db).await {
        Ok(_) => {
            tracing::info!(
                "E2EE: Loan request created: {} for '{}'",
                request_id,
                book_title
            );
            (
                StatusCode::OK,
                Json(json!({ "message": "Loan request received", "request_id": request_id })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save request: {e}") })),
        )
            .into_response(),
    }
}

/// Handle an encrypted loan confirmation (same logic as `receive_loan_confirmation` in peer.rs).
async fn handle_loan_confirmation(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> axum::response::Response {
    use crate::models::{book, copy};

    let title = msg
        .payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let isbn = msg
        .payload
        .get("isbn")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let author = msg
        .payload
        .get("author")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let cover_url = msg
        .payload
        .get("cover_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let lender_name = msg
        .payload
        .get("lender_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let due_date = msg
        .payload
        .get("due_date")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");

    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing title" })),
        )
            .into_response();
    }

    tracing::info!(
        "E2EE: Loan confirmation for '{}' from {}",
        title,
        lender_name
    );

    // Find or create book
    let existing_book = if let Some(ref isbn_val) = isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn_val))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(title))
            .one(db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => b.id,
        None => {
            let now = chrono::Utc::now().to_rfc3339();
            let summary_text = author.map(|a| format!("Auteur: {a}"));
            let new_book = book::ActiveModel {
                title: Set(title.to_string()),
                isbn: Set(isbn),
                summary: Set(summary_text),
                cover_url: Set(cover_url.clone()),
                owned: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            match new_book.insert(db).await {
                Ok(b) => b.id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create book: {e}") })),
                    )
                        .into_response();
                }
            }
        }
    };

    // Create borrowed copy
    let now = chrono::Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(1),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {lender_name} jusqu'au {due_date}"
        ))),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(db).await {
        Ok(c) => (
            StatusCode::OK,
            Json(json!({
                "message": "Loan confirmed",
                "book_id": book_id,
                "copy_id": c.id
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to create copy: {e}") })),
        )
            .into_response(),
    }
}

/// Handle a book sync request — return local books as JSON payload.
async fn handle_book_sync_request(db: &DatabaseConnection) -> serde_json::Value {
    use crate::models::book;

    let books = book::Entity::find().all(db).await.unwrap_or_default();
    let book_dtos: Vec<crate::models::Book> =
        books.into_iter().map(crate::models::Book::from).collect();
    json!({ "books": book_dtos })
}

/// Handle a search request — search local books and return results.
async fn handle_search_request(db: &DatabaseConnection, msg: &ClearMessage) -> serde_json::Value {
    use crate::models::book;

    let query = msg
        .payload
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let books = book::Entity::find()
        .filter(book::Column::Title.contains(query))
        .all(db)
        .await
        .unwrap_or_default();

    let book_dtos: Vec<crate::models::Book> =
        books.into_iter().map(crate::models::Book::from).collect();
    json!({ "results": book_dtos })
}

/// Handle a status update from a peer (loan status change notification).
async fn handle_status_update(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> axum::response::Response {
    use crate::models::p2p_outgoing_request;

    let loan_id = msg
        .payload
        .get("loan_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let status = msg
        .payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if loan_id.is_empty() || status.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing loan_id or status" })),
        )
            .into_response();
    }

    // Update the outgoing request status
    match p2p_outgoing_request::Entity::find_by_id(loan_id)
        .one(db)
        .await
    {
        Ok(Some(req)) => {
            let mut active: p2p_outgoing_request::ActiveModel = req.into();
            active.status = Set(status.to_string());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            match active.update(db).await {
                Ok(_) => {
                    tracing::info!("E2EE: Updated outgoing request {} to '{}'", loan_id, status);
                    (StatusCode::OK, Json(json!({ "message": "Status updated" }))).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update: {e}") })),
                )
                    .into_response(),
            }
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Request not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ── Response sealing helper ────────────────────────────────────────────

/// Seal a response payload back to the sender peer.
fn seal_response(
    crypto_service: &std::sync::Arc<
        crate::services::crypto_service::CryptoService<
            crate::infrastructure::nonce_store::SqliteNonceStore,
        >,
    >,
    sender_peer_info: &PeerInfo,
    message_type: &str,
    payload: serde_json::Value,
) -> axum::response::Response {
    let response_msg = ClearMessage {
        message_type: message_type.to_string(),
        payload,
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
    };

    match crypto_service.seal(&sender_peer_info.x25519_public, &response_msg) {
        Ok(envelope) => (
            StatusCode::OK,
            [(
                axum::http::header::HeaderName::from_static("x-e2ee"),
                axum::http::header::HeaderValue::from_static("true"),
            )],
            Json(envelope),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("E2EE: Failed to seal response: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to encrypt response" })),
            )
                .into_response()
        }
    }
}
