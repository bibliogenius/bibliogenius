//! Lender-side loan requests: receiving, listing, accepting or rejecting.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info};

/// Delete all non-pending incoming requests (cleanup).
pub async fn clear_incoming_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use sea_orm::ConnectionTrait;
    let result = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM p2p_requests WHERE status != 'pending'".to_owned(),
        ))
        .await;

    match result {
        Ok(r) => (
            StatusCode::OK,
            Json(json!({ "deleted": r.rows_affected() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct IncomingRequest {
    pub(crate) from_peer_url: String,
    pub(crate) from_peer_name: String,
    pub(crate) book_isbn: String,
    pub(crate) book_title: String,
    pub(crate) requester_request_id: Option<String>,
}

pub async fn receive_request(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<IncomingRequest>,
) -> impl IntoResponse {
    let db = state.db().clone();

    // 1. Find or Create Peer
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.from_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_peer_name),
                url: Set(payload.from_peer_url),
                // An unauthenticated POST must never mint a trusted, auto-approving peer.
                // A first-contact library is created pending: it is auto-approved for
                // nothing until the owner explicitly accepts it. The column default is
                // 'accepted', so this must be set, not left to `Default`. See ADR-050.
                connection_status: Set("pending".to_string()),
                auto_approve: Set(false),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // 2. Check copy availability and guard against duplicate active loans.
    let has_available_copy = {
        use crate::models::book;
        use crate::models::copy;

        let book_found = book::Entity::find()
            .filter(book::Column::Isbn.eq(&payload.book_isbn))
            .one(&db)
            .await
            .unwrap_or(None);

        if let Some(b) = book_found {
            copy::Entity::find()
                .filter(copy::Column::BookId.eq(b.id))
                .filter(copy::Column::Status.eq("available"))
                .one(&db)
                .await
                .unwrap_or(None)
                .is_some()
        } else {
            false
        }
    };

    // Guard: reject if this peer already has a pending or accepted request for this book
    //        (defense-in-depth — borrower side should catch this first).
    let already_has_active_request = {
        use crate::models::p2p_request;
        p2p_request::Entity::find()
            .filter(p2p_request::Column::FromPeerId.eq(peer.id))
            .filter(p2p_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(
                Condition::any()
                    .add(p2p_request::Column::Status.eq("pending"))
                    .add(p2p_request::Column::Status.eq("accepted")),
            )
            .one(&db)
            .await
            .unwrap_or(None)
            .is_some()
    };

    // 3. Check if auto-approve should be used
    let auto_approve =
        is_auto_approve_loans_enabled(&db).await && peer.connection_status == "accepted";

    // Determine initial status: auto-reject if no copy available or duplicate request
    let initial_status = if !has_available_copy || already_has_active_request {
        "rejected"
    } else {
        "pending"
    };

    // 4. Create Request Record
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = crate::models::p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set(initial_status.to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        requester_request_id: Set(payload.requester_request_id.clone()),
    };

    match crate::models::p2p_request::Entity::insert(request)
        .exec(&db)
        .await
    {
        Ok(_) => {
            // Auto-rejected: no available copy or duplicate active request
            if !has_available_copy || already_has_active_request {
                let reason = if already_has_active_request {
                    "already_borrowed"
                } else {
                    "no_available_copy"
                };
                tracing::info!(
                    "Auto-rejected loan request {} for '{}' - {}",
                    request_id,
                    payload.book_title,
                    reason
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "success": false, "status": "rejected", "reason": reason })),
                )
                    .into_response();
            }

            // If auto-approve is enabled, immediately accept the request
            if auto_approve {
                tracing::info!(
                    "Auto-approving loan request {} for peer {}",
                    request_id,
                    peer.name
                );
                match perform_loan_acceptance(
                    &db,
                    &request_id,
                    &payload.book_isbn,
                    &payload.book_title,
                    &peer,
                )
                .await
                {
                    Ok(result) => {
                        // Emit borrow_request notification (auto-approved)
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type: crate::domain::NotificationEventType::BorrowRequest,
                                title: payload.book_title.clone(),
                                body: Some(peer.name.clone()),
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(request_id.clone()),
                            },
                        )
                        .await;

                        // Fire-and-forget: try to notify borrower via E2EE (with relay fallback)
                        let confirm_payload = json!({
                            "isbn": result.book_isbn,
                            "title": result.book_title,
                            "cover_url": result.book_cover_url,
                            "lender_name": result.lender_name,
                            "due_date": result.due_date,
                            "request_id": request_id,
                            "requester_request_id": payload.requester_request_id,
                        });
                        let _ = try_send_e2ee(&state, &peer, "loan_confirmation", confirm_payload)
                            .await;

                        return (
                            StatusCode::OK,
                            Json(json!({
                                "success": true,
                                "status": "accepted",
                                "request_id": request_id,
                                "due_date": result.due_date,
                                "lender_name": result.lender_name,
                                "isbn": result.book_isbn,
                                "title": result.book_title,
                                "cover_url": result.book_cover_url,
                            })),
                        )
                            .into_response();
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Auto-approve failed for request {}: {} - staying pending",
                            request_id,
                            e
                        );
                        // Fall through to return "pending"
                    }
                }
            }

            // Emit borrow_request notification (only when NOT auto-approved)
            crate::services::notification_service::emit(
                &db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowRequest,
                    title: payload.book_title.clone(),
                    body: Some(peer.name.clone()),
                    ref_type: Some("peer".to_string()),
                    ref_id: Some(request_id.clone()),
                },
            )
            .await;

            (
                StatusCode::CREATED,
                Json(json!({ "success": true, "status": "pending" })),
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

pub async fn list_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::book;
    use crate::utils::cover_url::{self, ResolveScope};

    let requests = crate::models::p2p_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Collect unique ISBNs to look up local books
    let isbns: Vec<String> = requests
        .iter()
        .map(|(req, _)| req.book_isbn.clone())
        .filter(|isbn| !isbn.is_empty())
        .collect();

    // Cover URLs must be servable by `CachedNetworkImage` on the UI. A raw
    // filesystem path in `books.cover_url` (typical of a local upload before
    // hub sync) would render a placeholder on the owner's list of incoming
    // requests, so the rewrite to a hub URL or `/api/books/{id}/cover`
    // fallback happens here, keyed on the same LAN scope as `api/books.rs`.
    let hub_prefix = crate::models::Book::hub_cover_prefix(&db).await;
    let mut isbn_book_map: std::collections::HashMap<String, (String, Option<String>)> =
        std::collections::HashMap::new();
    if !isbns.is_empty()
        && let Ok(books) = book::Entity::find()
            .filter(book::Column::Isbn.is_in(isbns))
            .all(&db)
            .await
    {
        for b in books {
            if let Some(isbn) = &b.isbn {
                let resolved = cover_url::resolve_single(
                    b.cover_url.as_deref(),
                    &b.id,
                    Some(&b.updated_at),
                    hub_prefix.as_deref(),
                    ResolveScope::Lan,
                )
                .unwrap_or(None);
                isbn_book_map.insert(isbn.clone(), (b.id, resolved));
            }
        }
    }

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            let book_info = isbn_book_map.get(&req.book_isbn);
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "book_id": book_info.map(|(id, _)| id.clone()),
                "cover_url": book_info.and_then(|(_, url)| url.clone()),
                "status": req.status,
                "created_at": req.created_at,
                "updated_at": req.updated_at,
                "peer_id": peer.as_ref().map(|p| p.id),
                "peer_name": peer.as_ref().map(|p| p.name.clone()).unwrap_or("Unknown".to_string()),
                "peer_url": peer.map(|p| p.url)
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

#[derive(Deserialize)]
pub struct RequestAction {
    pub status: String,
}

pub async fn update_request_status(
    State(state): State<crate::infrastructure::AppState>,
    Path(id): Path<String>,
    Json(payload): Json<RequestAction>,
) -> impl IntoResponse {
    use crate::models::{book, contact, copy, loan, p2p_request};
    let db = state.db().clone();

    let req = match p2p_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(r)) => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response();
        }
    };

    let mut active: p2p_request::ActiveModel = req.clone().into();
    let new_status = payload.status.as_str();

    // State transition logic
    if new_status == "accepted" && req.status == "pending" {
        // 1. Find Peer to link/create Contact
        let peer = match peer::Entity::find_by_id(req.from_peer_id).one(&db).await {
            Ok(Some(p)) => p,
            _ => {
                // Peer no longer exists: auto-reject the request since we
                // cannot create the contact/loan without peer info.
                tracing::warn!(
                    "Peer {} not found for request {} - auto-rejecting",
                    req.from_peer_id,
                    req.id
                );
                active.status = Set("rejected".to_string());
                active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
                return (
                    StatusCode::OK,
                    Json(json!({ "message": "Request auto-rejected: peer no longer available" })),
                )
                    .into_response();
            }
        };

        // 2. Find Book and Available Copy
        tracing::info!(
            "Looking for book with ISBN: '{}' for request {}",
            req.book_isbn,
            req.id
        );
        let book = match book::Entity::find()
            .filter(book::Column::Isbn.eq(&req.book_isbn))
            .one(&db)
            .await
        {
            Ok(Some(b)) => {
                tracing::info!("Found book: {} (id={})", b.title, b.id);
                b
            }
            Ok(None) => {
                tracing::warn!(
                    "Book not found for ISBN: '{}'. Checking by title: '{}'",
                    req.book_isbn,
                    req.book_title
                );
                // Fallback: Try to find by title if ISBN lookup fails
                match book::Entity::find()
                    .filter(book::Column::Title.eq(&req.book_title))
                    .one(&db)
                    .await
                {
                    Ok(Some(b)) => {
                        tracing::info!("Found book by title: {} (id={})", b.title, b.id);
                        b
                    }
                    _ => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("Book not found (ISBN: '{}', Title: '{}')", req.book_isbn, req.book_title) })),
                        )
                            .into_response()
                    }
                }
            }
            Err(e) => {
                tracing::error!("DB error looking up book: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("DB error: {}", e) })),
                )
                    .into_response();
            }
        };

        let copy = match copy::Entity::find()
            .filter(copy::Column::BookId.eq(book.id.clone()))
            .filter(copy::Column::Status.eq("available"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            _ => {
                // Self-healing: Check if ANY copy exists
                let any_copy = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id.clone()))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if any_copy.is_none() {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No copy found" })),
                    )
                        .into_response();
                } else {
                    // Copies exist but none are available (truly borrowed)
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No available copies" })),
                    )
                        .into_response();
                }
            }
        };

        // 3. Find or Create Contact for Peer
        let contact = match contact::Entity::find()
            .filter(contact::Column::Name.eq(&peer.name))
            .filter(contact::Column::Type.eq("Library"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                // Create new contact
                let new_contact =
                    contact::ActiveModel {
                        r#type: Set("Library".to_string()),
                        name: Set(peer.name.clone()),
                        library_owner_id: Set(
                            match crate::utils::library_helpers::resolve_library_id(&db).await {
                                Ok(id) => id,
                                Err(e) => {
                                    return (
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        Json(json!({ "error": format!("No library: {}", e) })),
                                    )
                                        .into_response();
                                }
                            },
                        ),
                        is_active: Set(true),
                        created_at: Set(chrono::Utc::now().to_rfc3339()),
                        updated_at: Set(chrono::Utc::now().to_rfc3339()),
                        ..Default::default()
                    };
                match new_contact.insert(&db).await {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("Failed to create contact: {}", e) })),
                        )
                            .into_response();
                    }
                }
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "DB Error finding contact" })),
                )
                    .into_response();
            }
        };

        // 4. Create Loan
        let loan = loan::ActiveModel {
            copy_id: Set(copy.id.clone()),
            contact_id: Set(contact.id),
            library_id: Set(
                match crate::utils::library_helpers::resolve_library_id(&db).await {
                    Ok(id) => id,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("No library: {}", e) })),
                        )
                            .into_response();
                    }
                },
            ),
            loan_date: Set(chrono::Utc::now().to_rfc3339()),
            due_date: Set((chrono::Utc::now()
                + chrono::Duration::days(resolve_loan_duration_days(&db, &book.id).await))
            .to_rfc3339()),
            status: Set("active".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        if let Err(e) = loan::Entity::insert(loan).exec(&db).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create loan: {}", e) })),
            )
                .into_response();
        }

        // Update Copy status
        let mut active_copy: copy::ActiveModel = copy.into();
        active_copy.status = Set("loaned".to_string());
        info!(
            "Updating copy {} status to 'loaned' for loan acceptance",
            active_copy.id.clone().unwrap()
        );
        if let Err(e) = active_copy.update(&db).await {
            error!("Failed to update copy status to 'lent': {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to update copy status: {}", e) })),
            )
                .into_response();
        }

        // 5. Notify borrower that loan was accepted
        let peer_url = peer.url.clone();
        let book_isbn = book.isbn.clone();
        let book_title = book.title.clone();
        let hub_prefix = crate::models::Book::hub_cover_prefix(&db).await;
        // Borrower notification may travel via hub relay; use relay-safe
        // variant so unreachable local paths are stripped rather than sent.
        let book_cover = crate::models::Book::safe_cover_url_for_relay(
            book.cover_url.as_deref(),
            &book.id,
            Some(book.updated_at.as_str()),
            hub_prefix.as_deref(),
        );
        let due_date = (chrono::Utc::now()
            + chrono::Duration::days(resolve_loan_duration_days(&db, &book.id).await))
        .format("%Y-%m-%d")
        .to_string();

        // Get library name for lender identification
        let lender_name = crate::utils::library_helpers::resolve_lender_display_name(&db).await;

        let confirm_payload = serde_json::json!({
            "isbn": book_isbn,
            "title": book_title,
            "author": Option::<String>::None,
            "cover_url": book_cover,
            "lender_name": lender_name,
            "due_date": due_date,
            "request_id": req.id,
            "requester_request_id": req.requester_request_id,
        });

        // Try E2EE path first
        match try_send_e2ee(&state, &peer, "loan_confirmation", confirm_payload.clone()).await {
            Ok(Some(_)) => {
                tracing::info!("E2EE: Loan confirmation sent to {} (encrypted)", peer.name);
            }
            Err(e) => {
                // E2EE transport error — message MAY have been delivered.
                // Do NOT fall back to plaintext to avoid duplicate borrowed copies.
                tracing::warn!("E2EE: Loan confirmation error (no plaintext fallback): {e}");
            }
            Ok(None) => {
                // E2EE not available for this peer — fall back to plaintext
                let peer_url_clone = peer_url.clone();
                tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    let confirm_result = client
                        .post(format!("{}/api/peers/loans/confirm", peer_url_clone))
                        .json(&confirm_payload)
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await;

                    match confirm_result {
                        Ok(resp) => {
                            tracing::info!(
                                "Loan confirmation sent to {}: {}",
                                peer_url_clone,
                                resp.status()
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to send loan confirmation to {}: {}",
                                peer_url_clone,
                                e
                            );
                        }
                    }
                });
            }
        }
    } else if new_status == "returned" && req.status == "accepted" {
        // Handle Return
        // Find the loan associated with this peer (contact) and book
        // This is tricky because we didn't link Loan to Request directly.
        // We have to infer: Find active loan for this book's copy where contact matches peer.

        // 1. Find Peer/Contact (graceful: if peer is gone, still update request status)
        let peer_opt = peer::Entity::find_by_id(req.from_peer_id)
            .one(&db)
            .await
            .ok()
            .flatten();

        if peer_opt.is_none() {
            tracing::warn!(
                "Peer {} not found for return of request {} - updating request status only",
                req.from_peer_id,
                req.id
            );
        }

        let contact = match &peer_opt {
            Some(peer) => contact::Entity::find()
                .filter(contact::Column::Name.eq(&peer.name))
                .filter(contact::Column::Type.eq("Library"))
                .one(&db)
                .await
                .unwrap_or(None),
            None => None,
        };

        if let Some(contact) = contact {
            let book = book::Entity::find()
                .filter(book::Column::Isbn.eq(&req.book_isbn))
                .one(&db)
                .await
                .unwrap_or(None);

            if let Some(book) = book {
                // 3. Find Active Loan for any copy of this book for this contact
                let copies = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id.as_str()))
                    .all(&db)
                    .await
                    .unwrap_or(vec![]);

                let copy_ids: Vec<String> = copies.iter().map(|c| c.id.clone()).collect();

                let active_loan = loan::Entity::find()
                    .filter(loan::Column::ContactId.eq(contact.id))
                    .filter(loan::Column::Status.eq("active"))
                    .filter(loan::Column::CopyId.is_in(copy_ids))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if let Some(l) = active_loan {
                    let mut active_loan: loan::ActiveModel = l.clone().into();
                    active_loan.status = Set("returned".to_string());
                    active_loan.return_date = Set(Some(chrono::Utc::now().to_rfc3339()));
                    active_loan.updated_at = Set(chrono::Utc::now().to_rfc3339());
                    let _ = active_loan.update(&db).await;

                    // Update Copy
                    if let Some(copy) = copy::Entity::find_by_id(l.copy_id)
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                    {
                        let mut active_copy: copy::ActiveModel = copy.into();
                        active_copy.status = Set("available".to_string());
                        let _ = active_copy.update(&db).await;
                    }

                    // Emit book_returned notification
                    let peer_name = peer_opt
                        .as_ref()
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                    crate::services::notification_service::emit(
                        &db,
                        crate::domain::CreateNotification {
                            event_type: crate::domain::NotificationEventType::BookReturned,
                            title: book.title.clone(),
                            body: Some(peer_name),
                            ref_type: Some("loan".to_string()),
                            ref_id: Some(req.id.clone()),
                        },
                    )
                    .await;
                }
            }
        }
    }

    // Update Request Status
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    // Notify borrower of status change
    let peer_for_notify = peer::Entity::find_by_id(req.from_peer_id)
        .one(&db)
        .await
        .ok()
        .flatten();

    if let Some(peer) = peer_for_notify {
        // Use the borrower's original request ID so they can match
        // the status update to their outgoing request. Fall back to
        // our local ID for backward compat with old peers.
        let borrower_loan_id = req
            .requester_request_id
            .clone()
            .unwrap_or_else(|| req.id.clone());

        let status_payload = json!({
            "loan_id": borrower_loan_id,
            "status": new_status,
        });

        // Try E2EE first
        match try_send_e2ee(&state, &peer, "status_update", status_payload).await {
            Ok(Some(_)) => {
                tracing::info!("E2EE: Status update sent to {} (encrypted)", peer.name);
            }
            Err(e) => {
                // E2EE transport error — message MAY have been delivered.
                // Do NOT fall back to plaintext to avoid duplicate status updates.
                tracing::warn!("E2EE: Status update error (no plaintext fallback): {e}");
            }
            Ok(None) => {
                // E2EE not available for this peer — fall back to plaintext.
                // Assert our own identity so the borrower's ownership check accepts it:
                // the borrower resolves this uuid and requires it to name the lender.
                let peer_url = peer.url.clone();
                let request_id = borrower_loan_id;
                let status_to_send = new_status.to_string();
                let our_uuid = state.identity_service.library_uuid().map(|s| s.to_string());

                tokio::spawn(async move {
                    let client = get_safe_client();
                    let notify_url =
                        format!("{}/api/peers/requests/status/{}", peer_url, request_id);

                    tracing::info!(
                        "Notifying borrower {} of status change: {} -> {}",
                        peer_url,
                        request_id,
                        status_to_send
                    );

                    match client
                        .put(&notify_url)
                        .json(&serde_json::json!({
                            "status": status_to_send,
                            "library_uuid": our_uuid,
                        }))
                        .send()
                        .await
                    {
                        Ok(res) => {
                            tracing::info!("Borrower notified: {}", res.status());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to notify borrower: {}", e);
                        }
                    }
                });
            }
        }
    }

    match active.update(&db).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    match p2p_request::Entity::delete_by_id(id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Request not found" })),
                )
                    .into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct IncomingLoanRequest {
    pub from_name: String,
    pub from_url: String,
    pub library_uuid: Option<String>, // For P2P deduplication
    pub book_isbn: String,
    pub book_title: String,
}

pub async fn receive_loan_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingLoanRequest>,
) -> impl IntoResponse {
    use crate::models::p2p_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find or Create Peer (deduplicate by library_uuid if provided)
    let existing_peer = if let Some(ref uuid) = payload.library_uuid {
        // Try to find by UUID first (more stable)
        peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await
    } else {
        // Fallback to URL matching
        peer::Entity::find()
            .filter(peer::Column::Url.eq(&payload.from_url))
            .one(&db)
            .await
    };

    let peer = match existing_peer {
        Ok(Some(mut p)) => {
            // Update URL if changed (IP might have changed)
            if p.url != payload.from_url {
                tracing::info!(
                    "📝 Updating peer {} URL: {} -> {}",
                    p.name,
                    p.url,
                    payload.from_url
                );

                // Check for conflict: Does another peer already use this new URL?
                let conflict_peer = peer::Entity::find()
                    .filter(peer::Column::Url.eq(&payload.from_url))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if let Some(conflict) = conflict_peer {
                    // If conflict is NOT the same peer (ids differ), we have a problem.
                    // Since URLs must be unique and we trust the new incoming request (it's active right now),
                    // we assume the old entry holding this IP is stale.
                    if conflict.id != p.id {
                        tracing::warn!(
                            "⚠️ Found stale peer {} holding URL {}. Deleting it.",
                            conflict.name,
                            payload.from_url
                        );
                        let _ = peer::Entity::delete_by_id(conflict.id).exec(&db).await;
                    }
                }

                let mut active: peer::ActiveModel = p.clone().into();
                active.url = Set(payload.from_url.clone());
                active.updated_at = Set(Utc::now().to_rfc3339());
                if let Ok(updated) = active.update(&db).await {
                    p = updated;
                }
            }
            p
        }
        Ok(None) => {
            // Creating new peer. Check if URL is already taken by a stale peer (since UUID didn't match)
            let conflict_peer = peer::Entity::find()
                .filter(peer::Column::Url.eq(&payload.from_url))
                .one(&db)
                .await
                .unwrap_or(None);

            if let Some(conflict) = conflict_peer {
                tracing::warn!(
                    "⚠️ New peer claims URL {} held by old peer {}. Deleting old peer.",
                    payload.from_url,
                    conflict.name
                );
                let _ = peer::Entity::delete_by_id(conflict.id).exec(&db).await;
            }

            let new_peer = peer::ActiveModel {
                name: Set(payload.from_name),
                url: Set(payload.from_url),
                library_uuid: Set(payload.library_uuid),
                // Always pending: an unauthenticated inbound request never mints a
                // trusted, auto-approving peer, regardless of the connection_validation
                // toggle. We always validate here. See ADR-050.
                auto_approve: Set(false),
                connection_status: Set("pending".to_string()),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Create Incoming Request
    let request_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Loan request received", "request_id": request_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save request: {}", e) })),
        )
            .into_response(),
    }
}
