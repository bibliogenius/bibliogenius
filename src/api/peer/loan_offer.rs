//! Lender-initiated loan offers and P2P loan confirmations.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct OfferLoanRequest {
    pub book_id: Option<String>,
    pub book_isbn: Option<String>,
}

/// POST /api/peers/:id/offer-loan
///
/// Lender initiates a loan to a connected peer. Creates the loan locally and
/// notifies the peer via E2EE (with relay fallback) so a borrowed copy appears
/// on the borrower's device.
pub async fn offer_loan(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<OfferLoanRequest>,
) -> impl IntoResponse {
    use crate::models::{book, contact, copy, loan};

    let db = state.db();

    // 1. Find peer and verify connection status
    let peer = match peer::Entity::find_by_id(peer_id).one(db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };
    if peer.connection_status != "accepted" {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "Peer not connected" })),
        )
            .into_response();
    }

    // 2. Find book by ID or ISBN
    let book = if let Some(book_id) = payload.book_id {
        book::Entity::find_by_id(book_id)
            .one(db)
            .await
            .ok()
            .flatten()
    } else if let Some(ref isbn) = payload.book_isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let book = match book {
        Some(b) => b,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Book not found" })),
            )
                .into_response();
        }
    };

    // 3. Find available copy (no auto-creation)
    let available_copy = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book.id.clone()))
        .filter(copy::Column::Status.eq("available"))
        .one(db)
        .await
        .ok()
        .flatten();
    let available_copy = match available_copy {
        Some(c) => c,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "No available copies" })),
            )
                .into_response();
        }
    };

    // 4. Find or create Library contact for peer
    let peer_contact = match contact::Entity::find()
        .filter(contact::Column::Name.eq(&peer.name))
        .filter(contact::Column::Type.eq("Library"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
                Ok(id) => id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("No library: {e}") })),
                    )
                        .into_response();
                }
            };
            let new_contact = contact::ActiveModel {
                r#type: Set("Library".to_string()),
                name: Set(peer.name.clone()),
                library_owner_id: Set(lib_id),
                is_active: Set(true),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_contact.insert(db).await {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create contact: {e}") })),
                    )
                        .into_response();
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("DB error: {e}") })),
            )
                .into_response();
        }
    };

    // 5. Calculate loan duration and create loan
    let duration_days = resolve_loan_duration_days(db, &book.id).await;
    let due = Utc::now() + chrono::Duration::days(duration_days);
    let due_date_str = due.format("%Y-%m-%d").to_string();

    let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("No library: {e}") })),
            )
                .into_response();
        }
    };
    let new_loan = loan::ActiveModel {
        copy_id: Set(available_copy.id.clone()),
        contact_id: Set(peer_contact.id.clone()),
        library_id: Set(lib_id),
        loan_date: Set(Utc::now().to_rfc3339()),
        due_date: Set(due.to_rfc3339()),
        status: Set("active".to_string()),
        created_at: Set(Utc::now().to_rfc3339()),
        updated_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    // `insert` (ActiveModel) fires `before_save` to mint the uuid PK and returns
    // the model carrying it; `Entity::insert(..).exec().last_insert_id` would yield
    // the integer rowid, not the loan's uuid that the peer needs to reference it.
    let loan_insert = match new_loan.insert(db).await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create loan: {e}") })),
            )
                .into_response();
        }
    };

    // 6. Update copy status to "loaned"
    let mut active_copy: copy::ActiveModel = available_copy.into();
    active_copy.status = Set("loaned".to_string());
    if let Err(e) = active_copy.update(db).await {
        tracing::warn!("Failed to update copy status: {e}");
    }

    // 7. Create p2p_request so the return flow can find the loan
    let request_id = uuid::Uuid::new_v4().to_string();
    {
        use crate::models::p2p_request;
        let req = p2p_request::ActiveModel {
            id: Set(request_id.clone()),
            from_peer_id: Set(peer.id),
            book_isbn: Set(book.isbn.clone().unwrap_or_default()),
            book_title: Set(book.title.clone()),
            status: Set("accepted".to_string()),
            requester_request_id: Set(None),
            created_at: Set(Utc::now().to_rfc3339()),
            updated_at: Set(Utc::now().to_rfc3339()),
        };
        if let Err(e) = p2p_request::Entity::insert(req).exec(db).await {
            tracing::warn!("Failed to create p2p_request for offer_loan: {e}");
        }
    }

    // 8. Build loan_offer payload and notify peer
    let lender_name = crate::utils::library_helpers::resolve_lender_display_name(db).await;

    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    let offer_payload = json!({
        "isbn": book.isbn,
        "title": book.title,
        // Payload goes through `try_send_e2ee` (relay-capable): strip
        // unservable local paths rather than embedding a `/api/books/{id}/cover`
        // URL the borrower cannot reach from the hub relay.
        "cover_url": crate::models::Book::safe_cover_url_for_relay(
            book.cover_url.as_deref(),
            &book.id,
            Some(book.updated_at.as_str()),
            hub_prefix.as_deref(),
        ),
        "lender_name": lender_name,
        "due_date": due_date_str,
        "request_id": request_id,
        // Our stable identity. The plaintext endpoint has no authenticated sender,
        // so this is what lets the borrower resolve us to their local `peers` row
        // and notify us when they return the book. The E2EE path ignores it: there
        // the envelope already authenticates us.
        "library_uuid": state.identity_service.library_uuid(),
    });

    let mut notification_sent = false;

    match try_send_e2ee(&state, &peer, "loan_offer", offer_payload.clone()).await {
        Ok(Some(_)) => {
            tracing::info!("E2EE: loan_offer sent to {} (encrypted)", peer.name);
            notification_sent = true;
        }
        Err(e) => {
            tracing::warn!("E2EE: loan_offer error (no plaintext fallback): {e}");
        }
        Ok(None) => {
            // E2EE not available -- fall back to plaintext
            if let Ok(validated) = validate_url(&peer.url) {
                let client = get_safe_client();
                match client
                    .post(format!("{validated}/api/peers/loans/offer"))
                    .json(&offer_payload)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        tracing::info!(
                            "loan_offer plaintext sent to {}: {}",
                            peer.name,
                            resp.status()
                        );
                        notification_sent = resp.status().is_success();
                    }
                    Err(e) => {
                        tracing::warn!("Failed to send plaintext loan_offer to {}: {e}", peer.name);
                    }
                }
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "message": "Loan created",
            "loan_id": loan_insert.id,
            "contact_id": peer_contact.id,
            "due_date": due_date_str,
            "notification_sent": notification_sent,
        })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct LoanConfirmation {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub lender_name: String,
    pub due_date: String,
    pub request_id: Option<String>,
    /// Borrower's outgoing request ID (for precise confirmation matching)
    pub requester_request_id: Option<String>,
}

/// Receive loan confirmation from lender
/// Creates the book (if not exists) and a borrowed copy in the borrower's library
pub async fn receive_loan_confirmation(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoanConfirmation>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    tracing::info!(
        "📚 Received loan confirmation: '{}' from {} (requester_request_id={:?})",
        payload.title,
        payload.lender_name,
        payload.requester_request_id
    );

    // Guard: verify a matching pending outgoing request exists.
    // This prevents stale relay messages from creating orphan borrowed copies.
    let has_matching_request = if let Some(ref rr_id) = payload.requester_request_id {
        // Precise match by borrower's outgoing request ID
        p2p_outgoing_request::Entity::find_by_id(rr_id)
            .filter(p2p_outgoing_request::Column::Status.eq("pending"))
            .one(&db)
            .await
            .ok()
            .flatten()
            .is_some()
    } else {
        // Backward compat: old confirmations without requester_request_id - match by ISBN
        let isbn_filter = payload.isbn.clone().unwrap_or_default();
        if !isbn_filter.is_empty() {
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                .filter(p2p_outgoing_request::Column::Status.eq("pending"))
                .one(&db)
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
            "📚 No pending outgoing request for '{}' (requester_request_id={:?}, isbn={:?}), ignoring stale loan_confirmation",
            payload.title,
            payload.requester_request_id,
            payload.isbn
        );
        return (
            StatusCode::OK,
            Json(json!({ "message": "No pending request for this confirmation, ignored" })),
        )
            .into_response();
    }

    // The outgoing request we issued names the lender: `to_peer_id` is the peer we
    // sent the borrow request to. Resolved before the copy is created so the copy
    // can carry the back-reference (ADR-034), and reused below for the status
    // update rather than queried twice.
    // An empty ISBN must not be used as a search key: `book_isbn` is empty on every
    // outgoing request for a book that has no ISBN, so `eq("")` would match an
    // unrelated loan and name its peer as this lender. The guard above already
    // declines to match on an empty ISBN; this mirrors it.
    let outgoing = if let Some(ref rr_id) = payload.requester_request_id {
        p2p_outgoing_request::Entity::find_by_id(rr_id)
            .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
            .one(&db)
            .await
            .ok()
            .flatten()
    } else if let Some(isbn) = payload.isbn.as_deref().filter(|s| !s.is_empty()) {
        p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(isbn))
            .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
            .one(&db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    if outgoing.is_none() {
        tracing::warn!(
            "Loan confirmation '{}' from '{}' matches no outgoing request: \
             the borrowed copy carries no lender back-reference",
            payload.title,
            payload.lender_name,
        );
    }

    // Create borrowed copy via shared helper
    let params = BorrowedCopyParams {
        title: &payload.title,
        isbn: payload.isbn.as_deref(),
        author: payload.author.as_deref(),
        cover_url: payload.cover_url.as_deref(),
        lender_name: &payload.lender_name,
        due_date: &payload.due_date,
        lender_peer_id: outgoing.as_ref().map(|o| o.to_peer_id),
        // No library_uuid in a confirmation payload; `create_borrowed_copy`
        // resolves it from the peer row named by `lender_peer_id` (ADR-049).
        lender_library_uuid: None,
        lender_request_id: payload.request_id.as_deref(),
    };

    let result = match create_borrowed_copy(&db, &params).await {
        Ok(r) => r,
        Err((status, err_json)) => {
            return (status, Json(err_json)).into_response();
        }
    };

    // Update outgoing request with lender_request_id (both for idempotent and new copies)
    if let Some(ref lender_req_id) = payload.request_id
        && let Some(outgoing) = outgoing
    {
        let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
        active.lender_request_id = Set(Some(lender_req_id.clone()));
        active.status = Set("accepted".to_string());
        active.book_id = Set(Some(result.book_id.clone()));
        active.updated_at = Set(Utc::now().to_rfc3339());
        if let Err(e) = active.update(&db).await {
            tracing::warn!("Failed to update outgoing request: {e}");
        }
    }

    // Emit notification only for newly created copies
    if !result.already_existed {
        crate::services::notification_service::emit(
            &db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BorrowAccepted,
                title: payload.title.clone(),
                body: Some(payload.lender_name.clone()),
                ref_type: Some("peer".to_string()),
                ref_id: Some(result.copy_id.to_string()),
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

#[derive(Debug, Deserialize)]
pub struct LoanOffer {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub lender_name: String,
    pub due_date: String,
    /// Lender's p2p_request ID, needed for the return flow.
    pub request_id: Option<String>,
    /// Lender's stable library identifier, used to resolve their local `peers`
    /// row. Absent from offers sent by builds that predate this field, so the
    /// decoder tolerates its absence rather than rejecting the whole offer.
    pub library_uuid: Option<String>,
}

/// POST /api/peers/loans/offer -- Plaintext endpoint for receiving a loan offer.
///
/// Called when a lender initiates a loan to us (no prior borrow request).
/// Unlike `receive_loan_confirmation`, this does NOT require a matching
/// `p2p_outgoing_request` since the borrower never requested the loan.
pub async fn receive_loan_offer(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoanOffer>,
) -> impl IntoResponse {
    tracing::info!(
        "Received loan offer: '{}' from {}",
        payload.title,
        payload.lender_name
    );

    // We never issued a request for an offered loan, so there is no outgoing row
    // to read the lender's identity from: the payload's `library_uuid` is the only
    // handle on it. An unresolved lender degrades the loan rather than failing it.
    let lender = resolve_peer_by_library_uuid(&db, payload.library_uuid.as_deref()).await;

    let params = BorrowedCopyParams {
        title: &payload.title,
        isbn: payload.isbn.as_deref(),
        author: payload.author.as_deref(),
        cover_url: payload.cover_url.as_deref(),
        lender_name: &payload.lender_name,
        due_date: &payload.due_date,
        lender_peer_id: lender.as_ref().map(|p| p.id),
        // An offer carries the lender's library_uuid even when the peer is not
        // paired locally, so record it directly: it is what lets a second synced
        // device notify the lender on return (ADR-049).
        lender_library_uuid: payload.library_uuid.as_deref(),
        lender_request_id: payload.request_id.as_deref(),
    };

    let result = match create_borrowed_copy(&db, &params).await {
        Ok(r) => r,
        Err((status, err_json)) => {
            return (status, Json(err_json)).into_response();
        }
    };

    // Create p2p_outgoing_request so return_borrowed_book can notify the lender
    if !result.already_existed {
        match (&lender, &payload.request_id) {
            (Some(lender), Some(lender_req_id)) => {
                use crate::models::p2p_outgoing_request;
                let outgoing_id = uuid::Uuid::new_v4().to_string();
                let outgoing = p2p_outgoing_request::ActiveModel {
                    id: Set(outgoing_id),
                    to_peer_id: Set(lender.id),
                    book_isbn: Set(payload.isbn.clone().unwrap_or_default()),
                    book_title: Set(payload.title.clone()),
                    status: Set("accepted".to_string()),
                    lender_request_id: Set(Some(lender_req_id.clone())),
                    book_id: Set(Some(result.book_id.clone())),
                    created_at: Set(Utc::now().to_rfc3339()),
                    updated_at: Set(Utc::now().to_rfc3339()),
                };
                if let Err(e) = p2p_outgoing_request::Entity::insert(outgoing)
                    .exec(&db)
                    .await
                {
                    tracing::warn!("Failed to create p2p_outgoing_request for loan_offer: {e}");
                }
            }
            (None, _) => {
                tracing::warn!(
                    "Loan offer '{}' from '{}' carries no resolvable library_uuid ({:?}): \
                     the borrowed copy is created, but returning it cannot notify the lender",
                    payload.title,
                    payload.lender_name,
                    payload.library_uuid,
                );
            }
            (Some(_), None) => {
                tracing::warn!(
                    "Loan offer '{}' from '{}' carries no request_id: \
                     returning it cannot reference the lender's loan",
                    payload.title,
                    payload.lender_name,
                );
            }
        }

        crate::services::notification_service::emit(
            &db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BorrowAccepted,
                title: payload.title.clone(),
                body: Some(payload.lender_name.clone()),
                ref_type: Some("peer".to_string()),
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
