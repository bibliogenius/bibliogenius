//! E2EE message endpoint — single encrypted entry point for all peer-to-peer messages.
//!
//! `POST /api/e2ee/message` receives an `EncryptedEnvelope`, opens it,
//! and dispatches by `message_type` to existing business logic.

use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
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
    let (known_peers, peer_models) = build_known_peers(&peers);

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

    let sender_peer = &peer_models[peer_index];
    tracing::info!(
        "E2EE: Received '{}' from peer {} ({})",
        clear_message.message_type,
        sender_peer.name,
        sender_peer.id
    );

    // 5. Dispatch by message_type
    let our_uuid = state.identity_service.library_uuid().map(|s| s.to_string());
    dispatch_clear_message(
        db,
        &crypto_service,
        &clear_message,
        &known_peers,
        peer_index,
        sender_peer,
        our_uuid.as_deref(),
    )
    .await
}

/// Build PeerInfo vec from peer models with valid E2EE keys.
/// Returns (known_peers, peer_models) where indices are aligned.
pub fn build_known_peers(peers: &[peer::Model]) -> (Vec<PeerInfo>, Vec<peer::Model>) {
    let mut known_peers: Vec<PeerInfo> = Vec::new();
    let mut peer_models: Vec<peer::Model> = Vec::new();

    for p in peers {
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
                peer_models.push(p.clone());
            }
        }
    }

    (known_peers, peer_models)
}

/// Dispatch a decrypted ClearMessage to the appropriate handler.
/// Shared by both the HTTP endpoint and the relay poller.
pub async fn dispatch_clear_message(
    db: &sea_orm::DatabaseConnection,
    crypto_service: &std::sync::Arc<
        crate::services::crypto_service::CryptoService<
            crate::infrastructure::nonce_store::SqliteNonceStore,
        >,
    >,
    clear_message: &ClearMessage,
    known_peers: &[PeerInfo],
    peer_index: usize,
    sender_peer: &peer::Model,
    our_library_uuid: Option<&str>,
) -> axum::response::Response {
    match clear_message.message_type.as_str() {
        "loan_request" => handle_loan_request(db, sender_peer, clear_message).await,

        "loan_confirmation" => handle_loan_confirmation(db, clear_message).await,

        "book_sync_request" => {
            let response_payload = handle_book_sync_request(db).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "book_sync_response",
                response_payload,
            )
        }

        "search_request" => {
            let response_payload = handle_search_request(db, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "search_response",
                response_payload,
            )
        }

        "status_update" => handle_status_update(db, clear_message).await,

        "device_sync_request" => {
            let response_payload = handle_device_sync_request(db, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "device_sync_response",
                response_payload,
            )
        }

        "device_sync_push" => handle_device_sync_push(db, clear_message).await,

        "peer_disconnect" => handle_peer_disconnect(db, sender_peer, our_library_uuid).await,

        // ── Library sync via relay (ADR-012) ─────────────────────────
        "library_manifest_request" => {
            let response_payload = handle_library_manifest_request(db).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "library_manifest_response",
                response_payload,
            )
        }

        "library_page_request" => {
            let response_payload = handle_library_page_request(db, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "library_page_response",
                response_payload,
            )
        }

        "library_search_request" => {
            let response_payload = handle_library_search_via_relay(db, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "library_search_response",
                response_payload,
            )
        }

        // Response message types - these are handled by correlation matching
        // in the relay poller, not dispatched to handlers.
        "library_manifest_response" | "library_page_response" | "library_search_response" => {
            tracing::debug!(
                "E2EE: Received '{}' (handled by correlation)",
                clear_message.message_type
            );
            (
                StatusCode::OK,
                Json(json!({ "message": "Response processed by correlation" })),
            )
                .into_response()
        }

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

    let requester_request_id = msg
        .payload
        .get("requester_request_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

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
        requester_request_id: Set(requester_request_id),
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
    use crate::models::{book, copy, p2p_outgoing_request};

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

    let lender_request_id = msg
        .payload
        .get("request_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

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
                isbn: Set(isbn.clone()),
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
        Ok(c) => {
            tracing::info!(
                "E2EE: Created borrowed copy id={} for book_id={}",
                c.id,
                book_id
            );

            // Store lender_request_id on the matching outgoing request
            // (same logic as receive_loan_confirmation in peer.rs)
            if let Some(ref lender_req_id) = lender_request_id {
                let isbn_filter = isbn.clone().unwrap_or_default();
                if let Ok(Some(outgoing)) = p2p_outgoing_request::Entity::find()
                    .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                    .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                    .one(db)
                    .await
                {
                    let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
                    active.lender_request_id = Set(Some(lender_req_id.clone()));
                    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                    if let Err(e) = active.update(db).await {
                        tracing::warn!(
                            "E2EE: Failed to store lender_request_id on outgoing request: {e}"
                        );
                    } else {
                        tracing::info!(
                            "E2EE: Stored lender_request_id={} on outgoing request",
                            lender_req_id
                        );
                    }
                }
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Loan confirmed",
                    "book_id": book_id,
                    "copy_id": c.id
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to create copy: {e}") })),
        )
            .into_response(),
    }
}

/// Handle a book sync request - return local books as JSON payload.
pub async fn handle_book_sync_request(db: &DatabaseConnection) -> serde_json::Value {
    use crate::models::book;

    let books = book::Entity::find().all(db).await.unwrap_or_default();
    let book_dtos = crate::models::Book::populate_authors(db, books).await;
    json!({ "books": book_dtos })
}

/// Handle a search request - search local books and return results.
pub async fn handle_search_request(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
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

    let book_dtos = crate::models::Book::populate_authors(db, books).await;
    json!({ "results": book_dtos })
}

/// Handle a status update from a peer (loan status change notification).
///
/// This handler serves two directions:
/// - Lender → Borrower (accepted/rejected): updates `p2p_outgoing_request`
/// - Borrower → Lender (returned): updates `p2p_request` + loan + copy
async fn handle_status_update(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> axum::response::Response {
    use crate::models::{contact, copy, loan, p2p_outgoing_request, p2p_request, peer};

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

    // 1. Try borrower-side: update outgoing request (lender sent us accept/reject/returned)
    if let Ok(Some(req)) = p2p_outgoing_request::Entity::find_by_id(loan_id)
        .one(db)
        .await
    {
        let book_isbn = req.book_isbn.clone();
        let mut active: p2p_outgoing_request::ActiveModel = req.into();
        active.status = Set(status.to_string());
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        match active.update(db).await {
            Ok(_) => {
                tracing::info!("E2EE: Updated outgoing request {} to '{}'", loan_id, status);
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update: {e}") })),
                )
                    .into_response();
            }
        }

        // If lender reclaimed the book, clean up the borrowed copy + book
        if status == "returned"
            && let Ok(Some(bk)) = crate::models::book::Entity::find()
                .filter(crate::models::book::Column::Isbn.eq(&book_isbn))
                .one(db)
                .await
        {
            // Delete borrowed copies for this book
            let borrowed = copy::Entity::find()
                .filter(copy::Column::BookId.eq(bk.id))
                .filter(copy::Column::Status.eq("borrowed"))
                .all(db)
                .await
                .unwrap_or_default();
            for c in borrowed {
                let _ = copy::Entity::delete_by_id(c.id).exec(db).await;
            }

            // Clean up book if not owned, not wishlist, and no remaining copies
            if !bk.owned && bk.reading_status != "wanting" {
                let remaining = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(bk.id))
                    .count(db)
                    .await
                    .unwrap_or(1);
                if remaining == 0 {
                    let _ = crate::models::book::Entity::delete_by_id(bk.id)
                        .exec(db)
                        .await;
                    tracing::info!(
                        "E2EE: Cleaned up book (isbn={}) after lender reclaim",
                        book_isbn
                    );
                }
            }
        }

        return (StatusCode::OK, Json(json!({ "message": "Status updated" }))).into_response();
    }

    // 2. Try lender-side: update incoming request (borrower sent us return)
    let incoming = p2p_request::Entity::find_by_id(loan_id).one(db).await;
    match incoming {
        Ok(Some(req)) => {
            // Process return logic (same as update_request_status for "returned")
            if status == "returned" && req.status == "accepted" {
                // Find peer → contact → book → loan → mark returned + copy available
                if let Ok(Some(the_peer)) = peer::Entity::find_by_id(req.from_peer_id).one(db).await
                {
                    let the_contact = contact::Entity::find()
                        .filter(contact::Column::Name.eq(&the_peer.name))
                        .filter(contact::Column::Type.eq("Library"))
                        .one(db)
                        .await
                        .unwrap_or(None);

                    if let Some(the_contact) = the_contact {
                        let book = crate::models::book::Entity::find()
                            .filter(crate::models::book::Column::Isbn.eq(&req.book_isbn))
                            .one(db)
                            .await
                            .unwrap_or(None);

                        if let Some(book) = book {
                            let copies = copy::Entity::find()
                                .filter(copy::Column::BookId.eq(book.id))
                                .all(db)
                                .await
                                .unwrap_or_default();

                            let copy_ids: Vec<i32> = copies.iter().map(|c| c.id).collect();

                            let active_loan = loan::Entity::find()
                                .filter(loan::Column::ContactId.eq(the_contact.id))
                                .filter(loan::Column::Status.eq("active"))
                                .filter(loan::Column::CopyId.is_in(copy_ids))
                                .one(db)
                                .await
                                .unwrap_or(None);

                            if let Some(l) = active_loan {
                                let copy_id = l.copy_id;
                                let mut active_loan: loan::ActiveModel = l.into();
                                active_loan.status = Set("returned".to_string());
                                active_loan.return_date =
                                    Set(Some(chrono::Utc::now().to_rfc3339()));
                                active_loan.updated_at = Set(chrono::Utc::now().to_rfc3339());
                                let _ = active_loan.update(db).await;

                                if let Some(the_copy) = copy::Entity::find_by_id(copy_id)
                                    .one(db)
                                    .await
                                    .ok()
                                    .flatten()
                                {
                                    let mut active_copy: copy::ActiveModel = the_copy.into();
                                    active_copy.status = Set("available".to_string());
                                    let _ = active_copy.update(db).await;
                                }

                                tracing::info!(
                                    "E2EE: Processed return for request {} — loan + copy updated",
                                    loan_id
                                );
                            }
                        }
                    }
                }
            }

            // Update the incoming request status
            let mut active: p2p_request::ActiveModel = req.into();
            active.status = Set(status.to_string());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            match active.update(db).await {
                Ok(_) => {
                    tracing::info!("E2EE: Updated incoming request {} to '{}'", loan_id, status);
                    (StatusCode::OK, Json(json!({ "message": "Status updated" }))).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to update: {e}") })),
                )
                    .into_response(),
            }
        }
        Ok(None) => {
            tracing::warn!(
                "E2EE: status_update for unknown request {} (not in outgoing or incoming)",
                loan_id
            );
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ── Peer disconnect handler ────────────────────────────────────────────

/// Handle a disconnect notification from a remote peer (E2EE path).
///
/// The E2EE envelope already authenticates the sender. As an additional defense,
/// we perform a re-handshake to confirm the sender really initiated the disconnect.
/// For relay-only peers the re-handshake will timeout, which is acceptable since
/// E2EE authentication is already sufficient.
async fn handle_peer_disconnect(
    db: &DatabaseConnection,
    sender_peer: &peer::Model,
    our_library_uuid: Option<&str>,
) -> axum::response::Response {
    let peer_name = sender_peer.name.clone();
    let peer_id = sender_peer.id;

    // Re-handshake: confirm with the sender
    if let Some(uuid) = our_library_uuid {
        match crate::api::peer::verify_disconnect_with_peer(&sender_peer.url, uuid).await {
            Some(false) => {
                tracing::warn!(
                    "E2EE: Re-handshake failed - peer {} denied disconnect",
                    peer_name
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "Peer denied the disconnect" })),
                )
                    .into_response();
            }
            Some(true) | None => {
                // Confirmed or unreachable (relay-only peer) - proceed
            }
        }
    }

    match peer::Entity::delete_by_id(peer_id).exec(db).await {
        Ok(_) => {
            tracing::info!(
                "E2EE: Peer {} ({}) removed via disconnect notification",
                peer_name,
                peer_id
            );
            (
                StatusCode::OK,
                Json(json!({ "message": "Disconnect acknowledged" })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(
                "E2EE: Failed to delete peer {} after disconnect notification: {}",
                peer_id,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to remove peer: {}", e) })),
            )
                .into_response()
        }
    }
}

// ── Library sync handlers (ADR-012) ───────────────────────────────────

/// Handle a library manifest request - return catalog hash and book count.
/// Used for quick "has anything changed?" checks (like HTTP ETag).
pub async fn handle_library_manifest_request(db: &DatabaseConnection) -> serde_json::Value {
    use crate::models::book;
    use sha2::{Digest, Sha256};

    let books = book::Entity::find().all(db).await.unwrap_or_default();

    let total_books = books.len();

    // Compute catalog_hash: SHA-256 of sorted (id, updated_at) pairs
    let mut pairs: Vec<(i32, String)> =
        books.iter().map(|b| (b.id, b.updated_at.clone())).collect();
    pairs.sort_by_key(|(id, _)| *id);

    let mut hasher = Sha256::new();
    for (id, updated_at) in &pairs {
        hasher.update(format!("{id}:{updated_at}"));
    }
    let hash = hex::encode(hasher.finalize());

    let last_updated = books
        .iter()
        .map(|b| b.updated_at.as_str())
        .max()
        .unwrap_or("")
        .to_string();

    // Preview: up to 8 books with covers (shown before pages arrive)
    let preview_books: Vec<serde_json::Value> = {
        use sea_orm::QueryOrder;
        let with_covers = book::Entity::find()
            .filter(book::Column::CoverUrl.is_not_null())
            .order_by_desc(book::Column::UpdatedAt)
            .all(db)
            .await
            .unwrap_or_default();
        let preview: Vec<_> = with_covers.into_iter().take(8).collect();
        let preview_dtos = crate::models::Book::populate_authors(db, preview).await;
        preview_dtos
            .iter()
            .map(|b| {
                json!({
                    "id": b.id,
                    "title": b.title,
                    "author": b.author,
                    "isbn": b.isbn,
                    "cover_url": b.cover_url,
                })
            })
            .collect()
    };

    json!({
        "total_books": total_books,
        "catalog_hash": hash,
        "last_updated": last_updated,
        "preview_books": preview_books,
    })
}

/// Handle a library page request - return paginated books (browse profile).
/// Cursor-based pagination: { cursor: null|int, limit: 50 }
pub async fn handle_library_page_request(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::models::book;
    use sea_orm::QueryOrder;

    let cursor = msg
        .payload
        .get("cursor")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let limit = msg
        .payload
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .min(50) as usize;

    let mut query = book::Entity::find().order_by_asc(book::Column::Id);
    if let Some(c) = cursor {
        query = query.filter(book::Column::Id.gt(c));
    }

    let books = query.all(db).await.unwrap_or_default();

    let total = book::Entity::find().count(db).await.unwrap_or(0) as i64;

    let page: Vec<_> = books.into_iter().take(limit).collect();
    let next_cursor = page.last().map(|b| b.id);

    // Populate authors for browse profile
    let book_dtos = crate::models::Book::populate_authors(db, page).await;

    // Browse profile: only title, author, isbn, cover_url (~250 bytes/book)
    let browse_books: Vec<serde_json::Value> = book_dtos
        .iter()
        .map(|b| {
            json!({
                "id": b.id,
                "title": b.title,
                "author": b.author,
                "isbn": b.isbn,
                "cover_url": b.cover_url,
            })
        })
        .collect();

    json!({
        "books": browse_books,
        "next_cursor": next_cursor,
        "total": total,
    })
}

/// Handle a library search request via relay - search local books and return results.
/// Separate from handle_search_request to keep the existing one untouched.
pub async fn handle_library_search_via_relay(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::models::book;

    let query = msg
        .payload
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let limit = msg
        .payload
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(50) as usize;

    let books = book::Entity::find()
        .filter(book::Column::Title.contains(query))
        .all(db)
        .await
        .unwrap_or_default();

    let total_matches = books.len();
    let page: Vec<_> = books.into_iter().take(limit).collect();
    let book_dtos = crate::models::Book::populate_authors(db, page).await;

    // Browse profile
    let browse_books: Vec<serde_json::Value> = book_dtos
        .iter()
        .map(|b| {
            json!({
                "id": b.id,
                "title": b.title,
                "author": b.author,
                "isbn": b.isbn,
                "cover_url": b.cover_url,
            })
        })
        .collect();

    json!({
        "books": browse_books,
        "total_matches": total_matches,
    })
}

// ── Device sync handlers ──────────────────────────────────────────────

/// Check if sync safety mode is enabled (module "sync_safety" in enabled_modules).
async fn is_sync_safety_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile::ProfileConfig;

    match ProfileConfig::load(db).await {
        Ok(config) => config.is_module_enabled("sync_safety"),
        Err(_) => true, // Default to safe mode if profile can't be loaded
    }
}

/// Handle a device sync request (LAN, request-response).
///
/// Receives remote ops from the sender, stores them with appropriate status,
/// then returns our local ops since the sender's last sync point.
async fn handle_device_sync_request(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::services::device_sync_service::{DeviceSyncService, RemoteOp};

    let since = msg.payload.get("since").and_then(|v| v.as_str());

    let device_id = msg
        .payload
        .get("device_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let remote_ops: Vec<RemoteOp> = msg
        .payload
        .get("ops")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let safety_mode = is_sync_safety_enabled(db).await;

    // 1. Receive and store remote ops
    let repo: std::sync::Arc<dyn crate::domain::LinkedDeviceRepository> = std::sync::Arc::new(
        crate::infrastructure::SeaOrmLinkedDeviceRepository::new(db.clone()),
    );
    let svc = DeviceSyncService::new(db.clone(), repo.clone());

    let received_count = if !remote_ops.is_empty() {
        match svc
            .receive_remote_ops(device_id, remote_ops, safety_mode)
            .await
        {
            Ok(result) => result.inserted_count,
            Err(e) => {
                tracing::error!("E2EE: Failed to receive remote ops: {e}");
                0
            }
        }
    } else {
        0
    };

    // 2. Fetch our local ops since the given timestamp
    let local_ops = svc.get_local_ops_since(since).await.unwrap_or_default();

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

    // 3. Update last_synced on the device
    if device_id > 0 {
        let _ = svc
            .update_device_last_synced(device_id, &chrono::Utc::now().to_rfc3339())
            .await;
    }

    tracing::info!(
        "E2EE: device_sync_request - received {} ops, sending {} ops back",
        received_count,
        ops_payload.len()
    );

    json!({
        "ops": ops_payload,
        "received_count": received_count,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    })
}

/// Handle a device sync push (WAN/relay, fire-and-forget).
///
/// Receives remote ops and stores them. No response ops (relay is one-way).
async fn handle_device_sync_push(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> axum::response::Response {
    use crate::services::device_sync_service::{DeviceSyncService, RemoteOp};

    let device_id = msg
        .payload
        .get("device_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let remote_ops: Vec<RemoteOp> = msg
        .payload
        .get("ops")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    if remote_ops.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({ "message": "No ops to process" })),
        )
            .into_response();
    }

    let safety_mode = is_sync_safety_enabled(db).await;

    let repo: std::sync::Arc<dyn crate::domain::LinkedDeviceRepository> = std::sync::Arc::new(
        crate::infrastructure::SeaOrmLinkedDeviceRepository::new(db.clone()),
    );
    let svc = DeviceSyncService::new(db.clone(), repo);

    match svc
        .receive_remote_ops(device_id, remote_ops, safety_mode)
        .await
    {
        Ok(result) => {
            // Update last_synced
            if device_id > 0 {
                let _ = svc
                    .update_device_last_synced(device_id, &chrono::Utc::now().to_rfc3339())
                    .await;
            }

            tracing::info!(
                "E2EE: device_sync_push - stored {} ops from device {}",
                result.inserted_count,
                device_id
            );

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Sync push processed",
                    "received_count": result.inserted_count,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("E2EE: device_sync_push failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to process sync: {e}") })),
            )
                .into_response()
        }
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
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
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
