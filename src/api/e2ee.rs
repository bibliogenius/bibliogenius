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
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, Set,
};
use serde_json::json;

use crate::crypto::envelope::{ClearMessage, EncryptedEnvelope};
use crate::infrastructure::AppState;
use crate::models::{linked_device, peer};
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

    // 2b. Also load linked devices (device sync uses a separate table)
    let linked_devices = match linked_device::Entity::find().all(db).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("E2EE: Failed to load linked devices: {e}");
            vec![]
        }
    };

    // 3. Build PeerInfo vec from peers with valid keys + linked devices
    let (known_peers, peer_models) = build_known_peers_with_devices(&peers, &linked_devices);

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
        &state,
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

/// Build PeerInfo vec from both peers and linked devices.
/// Linked devices use raw binary keys instead of hex strings.
/// Returns synthetic peer::Model entries for linked devices so the dispatch
/// chain can log the sender name regardless of the source table.
pub fn build_known_peers_with_devices(
    peers: &[peer::Model],
    devices: &[linked_device::Model],
) -> (Vec<PeerInfo>, Vec<peer::Model>) {
    // Start with regular peers
    let (mut known_peers, mut peer_models) = build_known_peers(peers);

    // Add linked devices (binary keys, not hex)
    for d in devices {
        let ed_bytes = &d.ed25519_public_key;
        let x_bytes = &d.x25519_public_key;

        if ed_bytes.len() != 32 || x_bytes.len() != 32 {
            continue;
        }

        let ed_arr: [u8; 32] = match ed_bytes.as_slice().try_into() {
            Ok(a) => a,
            Err(_) => continue,
        };
        let x_arr: [u8; 32] = match x_bytes.as_slice().try_into() {
            Ok(a) => a,
            Err(_) => continue,
        };

        let verifying_key = match ed25519_dalek::VerifyingKey::from_bytes(&ed_arr) {
            Ok(k) => k,
            Err(_) => continue,
        };
        let x25519_public = x25519_dalek::PublicKey::from(x_arr);

        known_peers.push(PeerInfo {
            verifying_key,
            x25519_public,
        });
        // Synthesize a peer::Model so the dispatch chain can reference the sender
        peer_models.push(peer::Model {
            id: d.id,
            name: d.name.clone(),
            display_name: None,
            url: String::new(),
            library_uuid: None,
            public_key: Some(hex::encode(ed_bytes)),
            x25519_public_key: Some(hex::encode(x_bytes)),
            key_exchange_done: true,
            mailbox_id: d.mailbox_id.clone(),
            relay_url: d.relay_url.clone(),
            relay_write_token: d.relay_write_token.clone(),
            latitude: None,
            longitude: None,
            auto_approve: false,
            connection_status: "accepted".to_string(),
            last_seen: d.last_synced.clone(),
            avatar_config: None,
            catalog_hash: None,
            last_catalog_sync: None,
            last_delta_cursor: None,
            created_at: d.created_at.clone(),
            updated_at: d.created_at.clone(),
        });
    }

    (known_peers, peer_models)
}

/// Dispatch a decrypted ClearMessage to the appropriate handler.
/// Shared by both the HTTP endpoint and the relay poller.
pub async fn dispatch_clear_message(
    state: &crate::infrastructure::AppState,
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
    let db = state.db();
    match clear_message.message_type.as_str() {
        "loan_request" => {
            let response_payload =
                handle_loan_request_payload(db, sender_peer, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "loan_request_response",
                response_payload,
            )
        }

        "loan_confirmation" => handle_loan_confirmation(db, clear_message, sender_peer).await,

        "loan_offer" => handle_loan_offer(db, clear_message, sender_peer).await,

        "book_sync_request" => {
            let client_hash = clear_message
                .payload
                .get("catalog_hash")
                .and_then(|v| v.as_str());
            let response_payload = handle_book_sync_request(db, client_hash).await;
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

        "status_update" => handle_status_update(db, clear_message, sender_peer).await,

        "device_sync_request" => {
            let response_payload = handle_device_sync_request(db, clear_message, sender_peer).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "device_sync_response",
                response_payload,
            )
        }

        "device_sync_push" => handle_device_sync_push(db, clear_message, sender_peer).await,

        "peer_disconnect" => handle_peer_disconnect(db, sender_peer, our_library_uuid).await,

        // ── Library sync via relay (ADR-012) ─────────────────────────
        "library_manifest_request" => {
            let response_payload = handle_library_manifest_request(db, our_library_uuid).await;
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

        "library_browse_request" => {
            let response_payload = handle_library_browse_request(db, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "library_browse_response",
                response_payload,
            )
        }

        "request_status_query" => {
            let response_payload = handle_request_status_query(db, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "request_status_response",
                response_payload,
            )
        }

        // ── Delta sync via relay (ADR-029) ───────────────────────────
        "catalog_delta_request" => {
            let response_payload = handle_catalog_delta_request(state, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "catalog_delta_response",
                response_payload,
            )
        }

        // ── Avatar sync via relay (ADR-025) ──────────────────────────
        "avatar_sync_request" => {
            let response_payload = handle_avatar_sync_request(state, clear_message).await;
            seal_response(
                crypto_service,
                &known_peers[peer_index],
                "avatar_sync_response",
                response_payload,
            )
        }

        // Response message types - these are handled by correlation matching
        // in the relay poller, not dispatched to handlers.
        "library_manifest_response"
        | "library_page_response"
        | "library_search_response"
        | "library_browse_response"
        | "catalog_delta_response"
        | "avatar_sync_response" => {
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
/// Core loan request logic: validates, saves, returns request_id or error.
async fn save_loan_request(
    db: &DatabaseConnection,
    sender_peer: &peer::Model,
    msg: &ClearMessage,
) -> Result<(String, String), String> {
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
        return Err("Missing book_isbn or book_title".to_string());
    }

    let requester_request_id = msg
        .payload
        .get("requester_request_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Check copy availability before creating the request
    let has_available_copy = {
        use crate::models::{book, copy};

        let book_found = book::Entity::find()
            .filter(book::Column::Isbn.eq(book_isbn))
            .one(db)
            .await
            .unwrap_or(None);

        if let Some(b) = book_found {
            copy::Entity::find()
                .filter(copy::Column::BookId.eq(b.id))
                .filter(copy::Column::Status.eq("available"))
                .one(db)
                .await
                .unwrap_or(None)
                .is_some()
        } else {
            false
        }
    };

    // Guard: reject if this peer already has a pending or accepted request for this book
    //        (defense-in-depth — borrower side should catch this first).
    let already_has_active_request = p2p_request::Entity::find()
        .filter(p2p_request::Column::FromPeerId.eq(sender_peer.id))
        .filter(p2p_request::Column::BookIsbn.eq(book_isbn))
        .filter(
            Condition::any()
                .add(p2p_request::Column::Status.eq("pending"))
                .add(p2p_request::Column::Status.eq("accepted")),
        )
        .one(db)
        .await
        .unwrap_or(None)
        .is_some();

    let initial_status = if has_available_copy && !already_has_active_request {
        "pending"
    } else {
        "rejected"
    };

    let request_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let new_request = p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(sender_peer.id),
        book_isbn: Set(book_isbn.to_string()),
        book_title: Set(book_title.to_string()),
        status: Set(initial_status.to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        requester_request_id: Set(requester_request_id),
    };

    new_request
        .insert(db)
        .await
        .map_err(|e| format!("Failed to save request: {e}"))?;

    if initial_status == "rejected" {
        let reason = if already_has_active_request {
            "already_borrowed"
        } else {
            "no_available_copy"
        };
        tracing::info!(
            "E2EE: Loan request auto-rejected: {} for '{}' - {}",
            request_id,
            book_title,
            reason
        );
    } else {
        tracing::info!(
            "E2EE: Loan request created: {} for '{}'",
            request_id,
            book_title
        );
    }
    Ok((request_id, initial_status.to_string()))
}

/// Returns JSON payload for the loan request result (used by both E2EE and relay).
///
/// If auto-approve is enabled and the peer is accepted, immediately accepts the loan
/// and returns the acceptance details in the response so the borrower can process it
/// synchronously — no separate callback needed.
async fn handle_loan_request_payload(
    db: &DatabaseConnection,
    sender_peer: &peer::Model,
    msg: &ClearMessage,
) -> serde_json::Value {
    match save_loan_request(db, sender_peer, msg).await {
        Ok((request_id, status)) => {
            // Check auto-approve: if enabled and peer is accepted, accept inline
            if status == "pending"
                && crate::api::peer::is_auto_approve_loans_enabled(db).await
                && sender_peer.connection_status == "accepted"
            {
                tracing::info!(
                    "E2EE: Auto-approving loan request {} for peer {}",
                    request_id,
                    sender_peer.name
                );

                let book_title = msg
                    .payload
                    .get("book_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                match crate::api::peer::perform_loan_acceptance(
                    db,
                    &request_id,
                    msg.payload
                        .get("book_isbn")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                    book_title,
                    sender_peer,
                )
                .await
                {
                    Ok(result) => {
                        // Emit borrow_request notification (auto-approved)
                        crate::services::notification_service::emit(
                            db,
                            crate::domain::CreateNotification {
                                event_type: crate::domain::NotificationEventType::BorrowRequest,
                                title: book_title.to_string(),
                                body: Some(sender_peer.name.clone()),
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(request_id.clone()),
                            },
                        )
                        .await;

                        return json!({
                            "status": "accepted",
                            "request_id": request_id,
                            "due_date": result.due_date,
                            "lender_name": result.lender_name,
                            "isbn": result.book_isbn,
                            "title": result.book_title,
                            "cover_url": result.book_cover_url,
                            "message": "Loan request auto-approved",
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            "E2EE: Auto-approve failed for request {}: {} - staying pending",
                            request_id,
                            e
                        );
                        // Fall through to return "pending"
                    }
                }
            }

            // Emit borrow_request notification (only when NOT auto-approved)
            if status == "pending" {
                let book_title = msg
                    .payload
                    .get("book_title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                crate::services::notification_service::emit(
                    db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::BorrowRequest,
                        title: book_title.to_string(),
                        body: Some(sender_peer.name.clone()),
                        ref_type: Some("peer".to_string()),
                        ref_id: Some(request_id.clone()),
                    },
                )
                .await;
            }

            json!({ "request_id": request_id, "status": status, "message": "Loan request received" })
        }
        Err(e) => json!({ "error": e }),
    }
}

/// Relay variant: saves loan request and returns JSON payload for deposit.
pub async fn handle_loan_request_for_relay(
    db: &DatabaseConnection,
    sender_peer: &peer::Model,
    msg: &ClearMessage,
) -> serde_json::Value {
    handle_loan_request_payload(db, sender_peer, msg).await
}

/// Handle an encrypted loan confirmation (delegates to `create_borrowed_copy` helper).
async fn handle_loan_confirmation(
    db: &DatabaseConnection,
    msg: &ClearMessage,
    sender_peer: &peer::Model,
) -> axum::response::Response {
    use crate::models::p2p_outgoing_request;

    let title = msg
        .payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let isbn = msg.payload.get("isbn").and_then(|v| v.as_str());
    let author = msg.payload.get("author").and_then(|v| v.as_str());
    let cover_url = msg.payload.get("cover_url").and_then(|v| v.as_str());
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

    let lender_request_id = msg.payload.get("request_id").and_then(|v| v.as_str());

    let requester_request_id = msg
        .payload
        .get("requester_request_id")
        .and_then(|v| v.as_str());

    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing title" })),
        )
            .into_response();
    }

    tracing::info!(
        "E2EE: Loan confirmation for '{}' from {} (requester_request_id={:?})",
        title,
        lender_name,
        requester_request_id
    );

    // Guard: verify a matching pending outgoing request exists.
    // This prevents stale relay messages from creating orphan borrowed copies.
    let has_matching_request = if let Some(rr_id) = requester_request_id {
        p2p_outgoing_request::Entity::find_by_id(rr_id)
            .filter(p2p_outgoing_request::Column::Status.eq("pending"))
            .one(db)
            .await
            .ok()
            .flatten()
            .is_some()
    } else {
        // Backward compat: old confirmations without requester_request_id - match by ISBN
        let isbn_filter = isbn.unwrap_or_default();
        if !isbn_filter.is_empty() {
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookIsbn.eq(isbn_filter))
                .filter(p2p_outgoing_request::Column::Status.eq("pending"))
                .one(db)
                .await
                .ok()
                .flatten()
                .is_some()
        } else {
            // No ISBN, no requester_request_id - allow (best effort)
            true
        }
    };

    if !has_matching_request {
        tracing::warn!(
            "E2EE: No pending outgoing request for '{}' (requester_request_id={:?}, isbn={:?}), ignoring stale loan_confirmation",
            title,
            requester_request_id,
            isbn
        );
        return (
            StatusCode::OK,
            Json(json!({ "message": "No pending request for this confirmation, ignored" })),
        )
            .into_response();
    }

    // Create borrowed copy via shared helper
    let params = super::peer::BorrowedCopyParams {
        title,
        isbn,
        author,
        cover_url,
        lender_name,
        due_date,
    };

    let result = match super::peer::create_borrowed_copy(db, &params).await {
        Ok(r) => r,
        Err((status, err_json)) => {
            return (status, Json(err_json)).into_response();
        }
    };

    // Update outgoing request with lender_request_id
    if let Some(lender_req_id) = lender_request_id {
        let outgoing = if let Some(rr_id) = requester_request_id {
            p2p_outgoing_request::Entity::find_by_id(rr_id)
                .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                .one(db)
                .await
                .ok()
                .flatten()
        } else {
            let isbn_filter = isbn.unwrap_or_default();
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookIsbn.eq(isbn_filter))
                .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                .one(db)
                .await
                .ok()
                .flatten()
        };
        if let Some(outgoing) = outgoing {
            let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
            active.lender_request_id = Set(Some(lender_req_id.to_string()));
            active.status = Set("accepted".to_string());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            if let Err(e) = active.update(db).await {
                tracing::warn!("E2EE: Failed to update outgoing request: {e}");
            }
        }
    }

    // Emit notification only for newly created copies
    if !result.already_existed {
        crate::services::notification_service::emit(
            db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BorrowAccepted,
                title: title.to_string(),
                body: Some(sender_peer.name.clone()),
                ref_type: Some("loan".to_string()),
                ref_id: requester_request_id
                    .map(|s| s.to_string())
                    .or_else(|| lender_request_id.map(|s| s.to_string())),
            },
        )
        .await;
    }

    let msg = if result.already_existed {
        "Loan already confirmed"
    } else {
        "Loan confirmed"
    };
    (
        StatusCode::OK,
        Json(json!({
            "message": msg,
            "book_id": result.book_id,
            "copy_id": result.copy_id
        })),
    )
        .into_response()
}

/// Handle an encrypted loan offer (lender-initiated, no prior borrow request).
///
/// Same as `handle_loan_confirmation` but without the outgoing-request guard,
/// since the borrower never requested this loan.
async fn handle_loan_offer(
    db: &DatabaseConnection,
    msg: &ClearMessage,
    sender_peer: &peer::Model,
) -> axum::response::Response {
    let title = msg
        .payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let isbn = msg.payload.get("isbn").and_then(|v| v.as_str());
    let cover_url = msg.payload.get("cover_url").and_then(|v| v.as_str());
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

    let lender_request_id = msg.payload.get("request_id").and_then(|v| v.as_str());

    tracing::info!(
        "E2EE: Loan offer for '{}' from {} (request_id={:?})",
        title,
        lender_name,
        lender_request_id
    );

    let params = super::peer::BorrowedCopyParams {
        title,
        isbn,
        author: None,
        cover_url,
        lender_name,
        due_date,
    };

    let result = match super::peer::create_borrowed_copy(db, &params).await {
        Ok(r) => r,
        Err((status, err_json)) => {
            return (status, Json(err_json)).into_response();
        }
    };

    // Create p2p_outgoing_request so return_borrowed_book can notify the lender
    if !result.already_existed {
        if let Some(lender_req_id) = lender_request_id {
            use crate::models::p2p_outgoing_request;
            let outgoing_id = uuid::Uuid::new_v4().to_string();
            let outgoing = p2p_outgoing_request::ActiveModel {
                id: Set(outgoing_id),
                to_peer_id: Set(sender_peer.id),
                book_isbn: Set(isbn.unwrap_or_default().to_string()),
                book_title: Set(title.to_string()),
                status: Set("accepted".to_string()),
                lender_request_id: Set(Some(lender_req_id.to_string())),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
            };
            if let Err(e) = p2p_outgoing_request::Entity::insert(outgoing)
                .exec(db)
                .await
            {
                tracing::warn!("E2EE: Failed to create p2p_outgoing_request for loan_offer: {e}");
            }
        }

        crate::services::notification_service::emit(
            db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BorrowAccepted,
                title: title.to_string(),
                body: Some(sender_peer.name.clone()),
                ref_type: Some("loan".to_string()),
                ref_id: Some(result.copy_id.to_string()),
            },
        )
        .await;
    }

    let msg = if result.already_existed {
        "Loan offer already processed"
    } else {
        "Loan offer accepted"
    };
    (
        StatusCode::OK,
        Json(json!({
            "message": msg,
            "book_id": result.book_id,
            "copy_id": result.copy_id
        })),
    )
        .into_response()
}

/// Handle a book sync request - return local books as JSON payload.
///
/// Also includes `avatar_config` and `library_name` so relay-only peers (e.g. on 5G
/// where the LAN /api/config call would fail) can still update the peer's avatar and
/// display name from the relay response.
pub async fn handle_book_sync_request(
    db: &DatabaseConnection,
    client_catalog_hash: Option<&str>,
) -> serde_json::Value {
    use crate::models::book;

    // Canary first: if the requester is already in sync, send a tiny
    // "unchanged" payload (~80 bytes) instead of the full catalog (~95 KB
    // for a 110-book library). Sender side will skip its local cache
    // upsert when it sees this status.
    let current_hash = crate::models::Book::compute_catalog_hash(db).await;
    if let Some(client_hash) = client_catalog_hash
        && client_hash == current_hash
    {
        return json!({
            "status": "unchanged",
            "catalog_hash": current_hash,
        });
    }

    let books = book::Entity::find().all(db).await.unwrap_or_default();
    let mut book_dtos = crate::models::Book::populate_authors(db, books).await;
    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    crate::models::Book::rewrite_cover_urls_for_relay(&mut book_dtos, hub_prefix.as_deref());

    let avatar_config: Option<serde_json::Value> =
        crate::models::installation_profile::Entity::find_by_id(1)
            .one(db)
            .await
            .ok()
            .flatten()
            .and_then(|p| p.avatar_config)
            .and_then(|s| serde_json::from_str(&s).ok());

    let library_name: Option<String> = crate::models::library_config::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.name);

    let mut payload = json!({
        "status": "updated",
        "books": book_dtos,
        "catalog_hash": current_hash,
    });
    if let Some(avatar) = avatar_config {
        payload["avatar_config"] = avatar;
    }
    if let Some(name) = library_name {
        payload["library_name"] = json!(name);
    }
    payload
}

/// Handle a search request - search local books and return results.
pub async fn handle_search_request(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::models::book;
    use sea_orm::sea_query::Expr;

    let query = msg
        .payload
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let books = book::Entity::find()
        .filter(book::Column::Private.eq(false))
        .filter(
            Condition::any()
                .add(book::Column::Title.contains(query))
                .add(
                    Expr::col(book::Column::Id)
                        .in_subquery(crate::models::Book::author_search_subquery(query)),
                ),
        )
        .all(db)
        .await
        .unwrap_or_default();

    let mut book_dtos = crate::models::Book::populate_authors(db, books).await;
    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    crate::models::Book::rewrite_cover_urls_for_relay(&mut book_dtos, hub_prefix.as_deref());
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
    sender_peer: &peer::Model,
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
        let book_title = req.book_title.clone();
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

            // Emit book_reclaimed notification on borrower side (lender took the book back)
            crate::services::notification_service::emit(
                db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BookReclaimed,
                    title: bk.title.clone(),
                    body: Some(sender_peer.name.clone()),
                    ref_type: Some("loan".to_string()),
                    ref_id: Some(loan_id.to_string()),
                },
            )
            .await;
        }

        // Emit borrow_rejected notification on borrower side
        if status == "rejected" {
            crate::services::notification_service::emit(
                db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowRejected,
                    title: book_title.clone(),
                    body: Some(sender_peer.name.clone()),
                    ref_type: Some("loan".to_string()),
                    ref_id: Some(loan_id.to_string()),
                },
            )
            .await;
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

                                // Emit book_returned notification
                                crate::services::notification_service::emit(
                                    db,
                                    crate::domain::CreateNotification {
                                        event_type:
                                            crate::domain::NotificationEventType::BookReturned,
                                        title: book.title.clone(),
                                        body: Some(the_peer.name.clone()),
                                        ref_type: Some("loan".to_string()),
                                        ref_id: Some(loan_id.to_string()),
                                    },
                                )
                                .await;
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

// ── Delta sync handler (ADR-029) ──────────────────────────────────────

/// Handle a `catalog_delta_request` over E2EE (direct LAN or relay).
///
/// Mirrors the HTTP `list_books_delta` path: reuses `build_book_delta_response`
/// so any privacy / redaction change lands on both transports at once.
///
/// The response payload always carries `reset_required` (boolean) so the
/// requester can branch without inspecting envelope-level status codes. When
/// `reset_required=true`, `operations` is empty and `latest_cursor` echoes
/// the caller's cursor unchanged — the requester MUST fall back to the full
/// `library_manifest_request` loop (ADR-012) to rebuild state.
pub async fn handle_catalog_delta_request(
    state: &crate::infrastructure::AppState,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::api::books::BookDeltaOutcome;
    use crate::api::books::{DELTA_DEFAULT_LIMIT, DELTA_MAX_LIMIT};

    let since = msg.payload.get("since").and_then(|v| v.as_i64());
    let limit = msg
        .payload
        .get("limit")
        .and_then(|v| v.as_i64())
        .map(|n| n.clamp(1, DELTA_MAX_LIMIT))
        .unwrap_or(DELTA_DEFAULT_LIMIT);

    // Peer callers are never "owner" — the E1 privacy pipeline must run.
    // Relay cover-rewrite: peer may have no LAN route back (5G), so absolute
    // hub URLs (or `None`) must replace local `/api/books/{id}/cover` paths.
    let hub_prefix = crate::models::Book::hub_cover_prefix(state.db()).await;
    let cover_mode = crate::api::books::CoverRewriteMode::Relay { hub_prefix };
    let outcome =
        match crate::api::books::build_book_delta_response(state, since, limit, false, cover_mode)
            .await
        {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("catalog_delta_request: build_book_delta_response failed: {e}");
                return json!({
                    "operations": [],
                    "latest_cursor": since.unwrap_or(0),
                    "has_more": false,
                    "reset_required": false,
                    "error": "internal",
                });
            }
        };

    match outcome {
        BookDeltaOutcome::ResetRequired { current_cursor, .. } => json!({
            "operations": [],
            "latest_cursor": since.unwrap_or(0),
            "has_more": false,
            "reset_required": true,
            // Responder's current `operation_log` max id. The requester MUST
            // NOT adopt this as its new cursor until it has rebuilt state via
            // the legacy full-catalog flow; once that succeeds, persisting
            // this value breaks the reset loop so the next sync is a delta.
            // Additive field: old clients ignore it.
            "current_cursor": current_cursor,
        }),
        BookDeltaOutcome::Delta {
            operations,
            latest_cursor,
            has_more,
        } => json!({
            "operations": operations,
            "latest_cursor": latest_cursor,
            "has_more": has_more,
            "reset_required": false,
        }),
    }
}

// ── Avatar sync handler (ADR-025) ─────────────────────────────────────

/// Handle an `avatar_sync_request` over E2EE (direct LAN or relay).
///
/// Returns the local `installation_profile.avatar_config` (parsed JSON, or
/// null if absent / malformed) and `library_config.name` so the requester
/// can refresh both in a single round-trip. The request payload is empty
/// (`{}`), so no input validation is required — any shape is tolerated.
pub async fn handle_avatar_sync_request(
    state: &crate::infrastructure::AppState,
    _msg: &ClearMessage,
) -> serde_json::Value {
    let db = state.db();

    let avatar_config: Option<serde_json::Value> =
        crate::models::installation_profile::Entity::find_by_id(1)
            .one(db)
            .await
            .ok()
            .flatten()
            .and_then(|p| p.avatar_config)
            .and_then(|s| serde_json::from_str(&s).ok());

    let library_name: Option<String> = crate::models::library_config::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.name);

    json!({
        "avatar_config": avatar_config,
        "library_name": library_name,
    })
}

// ── Library sync handlers (ADR-012) ───────────────────────────────────

/// Handle a library manifest request - return catalog hash and book count.
/// Used for quick "has anything changed?" checks (like HTTP ETag).
/// Includes `library_uuid` so the requester can detect stale node IDs.
pub async fn handle_library_manifest_request(
    db: &DatabaseConnection,
    library_uuid: Option<&str>,
) -> serde_json::Value {
    use crate::models::book;

    let books = book::Entity::find().all(db).await.unwrap_or_default();

    let total_books = books.len();

    // Shared with `handle_book_sync_request` so manifest preview and full
    // sync agree on what "current catalog" means.
    let hash = crate::models::Book::compute_catalog_hash(db).await;

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
        let mut preview_dtos = crate::models::Book::populate_authors(db, preview).await;
        let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
        crate::models::Book::rewrite_cover_urls_for_relay(&mut preview_dtos, hub_prefix.as_deref());
        preview_dtos
            .iter()
            .map(|b| {
                json!({
                    "id": b.id,
                    "title": b.title,
                    "author": b.author,
                    "isbn": b.isbn,
                    "cover_url": b.cover_url,
                    "added_at": b.added_at,
                })
            })
            .collect()
    };

    // Include our library name so the requesting peer can update their
    // local record if we renamed (relay peers have no other sync path).
    let library_name = crate::models::library::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|lib| lib.name)
        .unwrap_or_default();

    json!({
        "total_books": total_books,
        "catalog_hash": hash,
        "last_updated": last_updated,
        "preview_books": preview_books,
        "library_name": library_name,
        "library_uuid": library_uuid,
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

    let mut query = book::Entity::find()
        .filter(book::Column::Private.eq(false))
        .order_by_asc(book::Column::Id);
    if let Some(c) = cursor {
        query = query.filter(book::Column::Id.gt(c));
    }

    let books = query.all(db).await.unwrap_or_default();

    let total = book::Entity::find().count(db).await.unwrap_or(0) as i64;

    let page: Vec<_> = books.into_iter().take(limit).collect();
    let next_cursor = page.last().map(|b| b.id);

    // Populate authors for browse profile
    let mut book_dtos = crate::models::Book::populate_authors(db, page).await;
    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    crate::models::Book::rewrite_cover_urls_for_relay(&mut book_dtos, hub_prefix.as_deref());

    // Browse profile: only title, author, isbn, cover_url, added_at
    // (~270 bytes/book). `added_at` is required for the "new" badge.
    let browse_books: Vec<serde_json::Value> = book_dtos
        .iter()
        .map(|b| {
            json!({
                "id": b.id,
                "title": b.title,
                "author": b.author,
                "isbn": b.isbn,
                "cover_url": b.cover_url,
                "added_at": b.added_at,
            })
        })
        .collect();

    json!({
        "books": browse_books,
        "next_cursor": next_cursor,
        "total": total,
    })
}

/// Handle a paginated library browse request via direct E2EE (offset-based).
/// Unlike `handle_library_page_request` (cursor-based, for relay), this uses
/// page/limit pagination matching the `/api/books` endpoint.
pub async fn handle_library_browse_request(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::models::book;
    use sea_orm::{PaginatorTrait, QueryFilter, QueryOrder};

    let page = msg
        .payload
        .get("page")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let limit = msg
        .payload
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(50);

    let query = book::Entity::find()
        .filter(book::Column::Owned.eq(true))
        .filter(book::Column::Private.eq(false))
        .order_by_asc(book::Column::ShelfPosition);

    let paginator = query.paginate(db, limit);
    let total = paginator.num_items().await.unwrap_or(0);
    let books = paginator.fetch_page(page).await.unwrap_or_default();

    let mut book_dtos = crate::models::Book::populate_authors(db, books).await;
    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    crate::models::Book::rewrite_cover_urls_for_relay(&mut book_dtos, hub_prefix.as_deref());
    let has_more = ((page + 1) * limit) < total;

    json!({
        "books": book_dtos,
        "total": total,
        "has_more": has_more,
    })
}

/// Handle a library search request via relay - search local books and return results.
/// Separate from handle_search_request to keep the existing one untouched.
pub async fn handle_library_search_via_relay(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::models::book;
    use sea_orm::sea_query::Expr;

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
        .filter(book::Column::Private.eq(false))
        .filter(
            Condition::any()
                .add(book::Column::Title.contains(query))
                .add(
                    Expr::col(book::Column::Id)
                        .in_subquery(crate::models::Book::author_search_subquery(query)),
                ),
        )
        .all(db)
        .await
        .unwrap_or_default();

    let total_matches = books.len();
    let page: Vec<_> = books.into_iter().take(limit).collect();
    let mut book_dtos = crate::models::Book::populate_authors(db, page).await;
    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    crate::models::Book::rewrite_cover_urls_for_relay(&mut book_dtos, hub_prefix.as_deref());

    // Browse profile (added_at needed for the "new" badge)
    let browse_books: Vec<serde_json::Value> = book_dtos
        .iter()
        .map(|b| {
            json!({
                "id": b.id,
                "title": b.title,
                "author": b.author,
                "isbn": b.isbn,
                "cover_url": b.cover_url,
                "added_at": b.added_at,
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
    sender_peer: &crate::models::peer::Model,
) -> serde_json::Value {
    use crate::services::device_sync_service::{DeviceSyncService, RemoteOp};

    let since = msg.payload.get("since").and_then(|v| v.as_str());

    // Use sender_peer.id which is the LOCAL linked_device ID.
    // (The synthetic peer::Model built from linked_device data carries the correct ID.)
    // The device_id in the request payload is the SENDER's local ID for us, which
    // does not match our local ID for the sender -- do not use it.
    let device_id = sender_peer.id;

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

    let mut ops_payload: Vec<serde_json::Value> = Vec::with_capacity(local_ops.len());
    for op in &local_ops {
        ops_payload.push(crate::sync::enrichment::op_to_sync_json(db, op).await);
    }

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
    sender_peer: &crate::models::peer::Model,
) -> axum::response::Response {
    use crate::services::device_sync_service::{DeviceSyncService, RemoteOp};

    // Use sender_peer.id (LOCAL linked_device ID from synthetic peer model)
    let device_id = sender_peer.id;

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

/// Handle a request_status_query: look up the local p2p_request by requester_request_id
/// and return its current status. This allows borrowers to poll for status changes
/// when asynchronous callbacks fail (e.g., cross-network scenarios).
pub async fn handle_request_status_query(
    db: &DatabaseConnection,
    msg: &ClearMessage,
) -> serde_json::Value {
    use crate::models::p2p_request;

    let requester_request_id = msg
        .payload
        .get("requester_request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if requester_request_id.is_empty() {
        return json!({ "error": "Missing requester_request_id" });
    }

    // Find the request by the borrower's outgoing ID
    let request = p2p_request::Entity::find()
        .filter(p2p_request::Column::RequesterRequestId.eq(requester_request_id))
        .one(db)
        .await
        .ok()
        .flatten();

    match request {
        Some(req) => {
            let mut response = json!({
                "requester_request_id": requester_request_id,
                "status": req.status,
            });

            // If accepted, include loan details so borrower can create the borrowed copy
            if req.status == "accepted" {
                // Get lender name and due date from the loan
                let lender_name = match crate::models::library::Entity::find_by_id(1).one(db).await
                {
                    Ok(Some(lib)) => lib.name,
                    _ => "Unknown Library".to_string(),
                };

                // Find the associated loan for due_date
                if let Ok(Some(book)) = crate::models::book::Entity::find()
                    .filter(crate::models::book::Column::Isbn.eq(&req.book_isbn))
                    .one(db)
                    .await
                {
                    response["isbn"] = json!(book.isbn);
                    response["title"] = json!(book.title);
                    response["cover_url"] = json!(book.cover_url);
                }

                response["lender_name"] = json!(lender_name);
                response["request_id"] = json!(req.id);

                // Find loan due_date via copy/loan chain
                if let Ok(Some(book)) = crate::models::book::Entity::find()
                    .filter(crate::models::book::Column::Isbn.eq(&req.book_isbn))
                    .one(db)
                    .await
                {
                    let copies = crate::models::copy::Entity::find()
                        .filter(crate::models::copy::Column::BookId.eq(book.id))
                        .all(db)
                        .await
                        .unwrap_or_default();
                    let copy_ids: Vec<i32> = copies.iter().map(|c| c.id).collect();
                    if !copy_ids.is_empty()
                        && let Ok(Some(loan)) = crate::models::loan::Entity::find()
                            .filter(crate::models::loan::Column::CopyId.is_in(copy_ids))
                            .filter(crate::models::loan::Column::Status.eq("active"))
                            .one(db)
                            .await
                    {
                        response["due_date"] = json!(loan.due_date);
                    }
                }
            }

            response
        }
        None => {
            json!({ "requester_request_id": requester_request_id, "status": "not_found" })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, IntoActiveModel, Set};

    async fn setup_test_db() -> sea_orm::DatabaseConnection {
        crate::db::init_db("sqlite::memory:")
            .await
            .expect("Failed to init in-memory DB")
    }

    /// Update the seeded installation_profile (id=1) to set an avatar_config.
    async fn set_avatar_config(db: &sea_orm::DatabaseConnection, avatar_json: &str) {
        use crate::models::installation_profile::Entity as ProfileEntity;
        let profile = ProfileEntity::find_by_id(1)
            .one(db)
            .await
            .expect("DB error")
            .expect("Default profile missing");
        let mut active = profile.into_active_model();
        active.avatar_config = Set(Some(avatar_json.to_string()));
        active
            .save(db)
            .await
            .expect("Failed to update avatar_config");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn book_sync_response_includes_avatar_and_library_name() {
        let db = setup_test_db().await;
        // init_db seeds installation_profile (id=1, no avatar) and library_config (id=1, "My Library")
        let avatar_json = r#"{"style":"lorelei","seed":"alice"}"#;
        set_avatar_config(&db, avatar_json).await;

        let response = handle_book_sync_request(&db, None).await;

        assert!(
            response.get("avatar_config").is_some(),
            "book_sync_request response must include avatar_config for relay sync"
        );
        assert_eq!(
            response["avatar_config"]["style"].as_str(),
            Some("lorelei"),
            "avatar_config style must match what was stored"
        );
        assert_eq!(
            response["library_name"].as_str(),
            Some("My Library"),
            "book_sync_request response must include library_name"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn book_sync_response_without_avatar_omits_avatar_field() {
        let db = setup_test_db().await;
        // init_db seeds installation_profile with no avatar_config (NULL)
        let response = handle_book_sync_request(&db, None).await;
        assert!(
            response.get("avatar_config").is_none(),
            "avatar_config must be absent when installation_profile has no avatar set"
        );
        // library_config is seeded with "My Library" - library_name must always be present
        assert!(
            response.get("library_name").is_some(),
            "library_name must be present (seeded by init_db)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn book_sync_response_includes_catalog_hash_on_full_sync() {
        let db = setup_test_db().await;
        let response = handle_book_sync_request(&db, None).await;
        let hash = response
            .get("catalog_hash")
            .and_then(|v| v.as_str())
            .expect("catalog_hash must be present on a full response");
        assert_eq!(hash.len(), 64, "expected lowercase hex SHA-256 (64 chars)");
        assert_eq!(
            response.get("status").and_then(|v| v.as_str()),
            Some("updated"),
            "full response must report status=updated",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn book_sync_response_short_circuits_when_client_hash_matches() {
        let db = setup_test_db().await;
        // Capture the current hash exactly as the responder computes it.
        let current_hash = crate::models::Book::compute_catalog_hash(&db).await;

        let response = handle_book_sync_request(&db, Some(&current_hash)).await;

        assert_eq!(
            response.get("status").and_then(|v| v.as_str()),
            Some("unchanged"),
            "matching hash must return status=unchanged",
        );
        assert_eq!(
            response.get("catalog_hash").and_then(|v| v.as_str()),
            Some(current_hash.as_str()),
            "unchanged response must echo the current catalog_hash",
        );
        assert!(
            response.get("books").is_none(),
            "unchanged response must not embed the book list (the whole point)",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn book_sync_response_does_full_sync_when_client_hash_stale() {
        let db = setup_test_db().await;
        let response = handle_book_sync_request(&db, Some("stale-hash-from-yesterday")).await;
        assert_eq!(
            response.get("status").and_then(|v| v.as_str()),
            Some("updated"),
            "non-matching hash must trigger a full sync",
        );
        assert!(
            response.get("books").is_some(),
            "full sync must include the book list",
        );
    }
}
