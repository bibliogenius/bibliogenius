//! Borrower-side loan requests: sending, tracking, cancelling.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use serde::Deserialize;
use serde_json::json;

/// Borrower-side: process an auto-approve acceptance from the lender's synchronous response.
///
/// Updates the outgoing request to "accepted" and creates a borrowed copy in the local library.
/// Called from both E2EE and plaintext paths when the lender auto-accepts.
pub(crate) async fn process_borrower_acceptance(
    db: &DatabaseConnection,
    outgoing_id: &str,
    payload: &serde_json::Value,
    lender_request_id: Option<&str>,
) {
    use crate::models::{book, copy, p2p_outgoing_request};

    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
    // An empty ISBN is not an ISBN: it would match every book row storing the empty
    // string, and writing it back seeds the next collision. Normalize once, here.
    let isbn = payload
        .get("isbn")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let cover_url = payload
        .get("cover_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let lender_name = payload
        .get("lender_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let due_date = payload
        .get("due_date")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");

    if title.is_empty() {
        tracing::warn!("process_borrower_acceptance: empty title, skipping");
        return;
    }

    // 1. Update outgoing request to "accepted"
    if let Ok(Some(outgoing)) = p2p_outgoing_request::Entity::find_by_id(outgoing_id)
        .one(db)
        .await
    {
        let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
        active.status = Set("accepted".to_string());
        if let Some(lr_id) = lender_request_id {
            active.lender_request_id = Set(Some(lr_id.to_string()));
        }
        active.updated_at = Set(Utc::now().to_rfc3339());
        let _ = active.update(db).await;
    }

    // 2. Find or create book
    let existing_book = if let Some(isbn_val) = isbn {
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
            let now = Utc::now().to_rfc3339();
            let new_book = book::ActiveModel {
                title: Set(title.to_string()),
                isbn: Set(isbn.map(|s| s.to_string())),
                cover_url: Set(cover_url.clone()),
                owned: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            match new_book.insert(db).await {
                Ok(b) => b.id,
                Err(e) => {
                    tracing::error!("process_borrower_acceptance: failed to create book: {e}");
                    return;
                }
            }
        }
    };

    // 2b. Link the outgoing request to the local book now that it exists. The status
    // update above runs before the book is resolved, and several early returns sit
    // in between, so this is a second, narrow write rather than a reordering.
    // Without it the lender-reclaim path would fall back to resolving by ISBN.
    // The same row also names the lender: `to_peer_id` is the peer we sent the
    // borrow request to, so the copy below can carry the back-reference (ADR-034).
    let mut lender_peer_id = None;
    if let Ok(Some(outgoing)) = p2p_outgoing_request::Entity::find_by_id(outgoing_id)
        .one(db)
        .await
    {
        lender_peer_id = Some(outgoing.to_peer_id);
        let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
        active.book_id = Set(Some(book_id.clone()));
        active.updated_at = Set(Utc::now().to_rfc3339());
        if let Err(e) = active.update(db).await {
            tracing::warn!("process_borrower_acceptance: failed to link book_id: {e}");
        }
    } else {
        tracing::warn!(
            "process_borrower_acceptance: outgoing request {outgoing_id} not found; \
             the borrowed copy carries no lender back-reference"
        );
    }

    // 3. Idempotency: skip if this lender already lent us a copy of this book
    if find_peer_borrowed_copy(db, &book_id, lender_peer_id)
        .await
        .is_some()
    {
        tracing::info!(
            "process_borrower_acceptance: borrowed copy already exists for book_id={} from lender {:?}",
            book_id,
            lender_peer_id
        );
        return;
    }

    // 4. Create borrowed copy
    let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("process_borrower_acceptance: failed to resolve library: {e}");
            return;
        }
    };
    let now = Utc::now().to_rfc3339();
    // ADR-034: typed loan columns only; `notes` freed for user notes.
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id.clone()),
        library_id: Set(lib_id),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        lender_display_name: Set(Some(lender_name.to_string())),
        lender_peer_id: Set(lender_peer_id),
        borrow_due_date: Set(Some(due_date.to_string())),
        borrow_source: Set(Some(crate::domain::BorrowSource::Peer.as_str().to_string())),
        acquisition_date: Set(Some(now.clone())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(db).await {
        Ok(c) => {
            tracing::info!(
                "process_borrower_acceptance: created borrowed copy id={} for book_id={}",
                c.id,
                book_id
            );
            // Notify the borrower that the loan was accepted
            crate::services::notification_service::emit(
                db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowAccepted,
                    title: title.to_string(),
                    body: Some(lender_name.to_string()),
                    ref_type: Some("peer".to_string()),
                    ref_id: None,
                },
            )
            .await;
        }
        Err(e) => {
            tracing::error!("process_borrower_acceptance: failed to create copy: {e}");
        }
    }
}

#[derive(Deserialize)]
pub struct BookRequest {
    book_isbn: String,
    book_title: String,
}

pub async fn request_book(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<BookRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find peer
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

    // 2. Guards: prevent invalid borrow requests.
    {
        use crate::models::{p2p_outgoing_request, p2p_request};

        // 2a. Reject if there is already a pending or accepted outgoing request for this
        //     book from this peer (prevents double-borrowing the same copy).
        let already_borrowing = p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::ToPeerId.eq(peer.id))
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(
                Condition::any()
                    .add(p2p_outgoing_request::Column::Status.eq("pending"))
                    .add(p2p_outgoing_request::Column::Status.eq("accepted")),
            )
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        // 2b. Reject if user is currently lending this book to the same peer
        //     (prevents borrowing back a book that is out on loan to them).
        let currently_lending = p2p_request::Entity::find()
            .filter(p2p_request::Column::FromPeerId.eq(peer.id))
            .filter(p2p_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(p2p_request::Column::Status.eq("accepted"))
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        if already_borrowing || currently_lending {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "already_requested" })),
            )
                .into_response();
        }
    }

    // 3. Save Outgoing Request
    let outgoing_id = uuid::Uuid::new_v4().to_string();
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(outgoing_id.clone()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        lender_request_id: Set(None),
        // No local book row exists yet: it is created together with the
        // borrowed copy once the lender confirms the loan.
        book_id: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(db)
        .await
    {
        tracing::error!("❌ Failed to save outgoing status: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 4. Send request to peer
    // Note: validate_url is deferred to the plaintext fallback path below.
    // Relay-only peers have a relay:// URL that is valid for E2EE but not for
    // direct HTTP, so SSRF validation must not block the E2EE path.

    // Try E2EE path first
    let my_config = match crate::models::library_config::Entity::find().one(db).await {
        Ok(Some(config)) => config,
        _ => {
            tracing::error!("❌ Library config not found when sending request");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response();
        }
    };

    let e2ee_payload = json!({
        "from_peer_url": state.our_public_url(),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(response)) => {
            // Check lender's synchronous response for auto-reject or auto-accept
            if let Some(ref clear_msg) = response {
                let status = clear_msg
                    .payload
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");

                if status == "rejected" {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (E2EE)",
                        outgoing_id
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": "no_available_copy" })),
                    )
                        .into_response();
                }

                if status == "accepted" {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (E2EE)",
                        outgoing_id
                    );
                    // Process acceptance: update outgoing request + create borrowed copy
                    let lender_request_id = clear_msg
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    process_borrower_acceptance(
                        db,
                        &outgoing_id,
                        &clear_msg.payload,
                        lender_request_id.as_deref(),
                    )
                    .await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
            }
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)", "status": "pending" })),
            )
                .into_response();
        }
        Ok(None) => {
            // Peer doesn't support E2EE, fall through to plaintext
        }
        Err(e) => {
            // E2EE transport error - both direct and relay failed.
            // Do NOT fall back to plaintext to avoid duplicate requests.
            tracing::warn!("E2EE send failed (no plaintext fallback): {}", e);
            crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to deliver request to peer" })),
            )
                .into_response();
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    if let Err(e) = validate_url(&peer.url) {
        crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Cannot reach peer: {}", e) })),
        )
            .into_response();
    }
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            let resp_status = response.status();
            let body = response.text().await.unwrap_or_default();

            if resp_status.is_success() {
                // Parse response body to check for auto-acceptance
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("accepted")
                {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (plaintext)",
                        outgoing_id
                    );
                    let lender_request_id = parsed.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(db, &outgoing_id, &parsed, lender_request_id).await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Request sent", "status": "pending" })),
                )
                    .into_response()
            } else {
                // Parse lender response to check for auto-rejection
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("rejected")
                {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    let reason = parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (plaintext): {}",
                        outgoing_id,
                        reason
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": reason })),
                    )
                        .into_response();
                }
                crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => {
            crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to contact peer" })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct BookRequestByUrl {
    peer_url: String,
    book_isbn: String,
    book_title: String,
}

pub async fn request_book_by_url(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<BookRequestByUrl>,
) -> impl IntoResponse {
    let db = state.db();

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(&payload.peer_url);

    // 1. Find peer by URL (may be None for unsaved mDNS peers)
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(db)
        .await
    {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // Unsaved mDNS peer: skip outgoing request tracking, send plaintext directly.
    // SSRF defense (ADR-026): route through ensure_registered_peer_or_mdns so
    // the fallback traversal is logged on the ssrf:mdns target. With
    // allow_unregistered_lan=true, a missing peer row yields Ok(None) and the
    // plaintext request proceeds; registered peers are handled on the main
    // branch above, so only Ok(None) is expected here in practice.
    if peer.is_none() {
        if let Err(e) = validate_url(&docker_url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }
        if let Err(status) = ensure_registered_peer_or_mdns(db, &docker_url, true).await {
            return status.into_response();
        }

        let my_config = match crate::models::library_config::Entity::find().one(db).await {
            Ok(Some(config)) => config,
            _ => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Library config not found" })),
                )
                    .into_response();
            }
        };

        let request_payload = json!({
            "from_peer_url": state.our_public_url(),
            "from_peer_name": my_config.name,
            "book_isbn": payload.book_isbn,
            "book_title": payload.book_title,
            "requester_request_id": uuid::Uuid::new_v4().to_string()
        });

        let client = get_safe_client();
        let url = format!("{}/api/peers/request", docker_url);
        return match client.post(&url).json(&request_payload).send().await {
            Ok(response) if response.status().is_success() => {
                (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response()
            }
            Ok(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Peer rejected request" })),
            )
                .into_response(),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to contact peer" })),
            )
                .into_response(),
        };
    }

    let peer = peer.unwrap();

    // 2. Guards: prevent invalid borrow requests.
    {
        use crate::models::{p2p_outgoing_request, p2p_request};

        // 2a. Reject if there is already a pending or accepted outgoing request for this
        //     book from this peer (prevents double-borrowing the same copy).
        let already_borrowing = p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::ToPeerId.eq(peer.id))
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(
                Condition::any()
                    .add(p2p_outgoing_request::Column::Status.eq("pending"))
                    .add(p2p_outgoing_request::Column::Status.eq("accepted")),
            )
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        // 2b. Reject if user is currently lending this book to the same peer
        //     (prevents borrowing back a book that is out on loan to them).
        let currently_lending = p2p_request::Entity::find()
            .filter(p2p_request::Column::FromPeerId.eq(peer.id))
            .filter(p2p_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(p2p_request::Column::Status.eq("accepted"))
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        if already_borrowing || currently_lending {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "already_requested" })),
            )
                .into_response();
        }
    }

    // 3. Save Outgoing Request
    let outgoing_id = uuid::Uuid::new_v4().to_string();
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(outgoing_id.clone()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        lender_request_id: Set(None),
        // No local book row exists yet: it is created together with the
        // borrowed copy once the lender confirms the loan.
        book_id: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(db)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 4. Send request to peer
    // Note: validate_url is deferred to the plaintext fallback path below.
    // Relay-only peers have a relay:// URL that is valid for E2EE but not for
    // direct HTTP, so SSRF validation must not block the E2EE path.

    // Get my config to identify myself
    let my_config = match crate::models::library_config::Entity::find().one(db).await {
        Ok(Some(config)) => config,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response();
        }
    };

    let e2ee_payload = json!({
        "from_peer_url": state.our_public_url(),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    // Try E2EE path first
    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(response)) => {
            // Check lender's synchronous response for auto-reject or auto-accept
            if let Some(ref clear_msg) = response {
                let status = clear_msg
                    .payload
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");
                if status == "rejected" {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (E2EE)",
                        outgoing_id
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": "no_available_copy" })),
                    )
                        .into_response();
                }

                if status == "accepted" {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (E2EE)",
                        outgoing_id
                    );
                    let lender_request_id = clear_msg
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    process_borrower_acceptance(
                        db,
                        &outgoing_id,
                        &clear_msg.payload,
                        lender_request_id.as_deref(),
                    )
                    .await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
            }
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)", "status": "pending" })),
            )
                .into_response();
        }
        Ok(None) => {
            // E2EE not available for this peer — fall back to plaintext.
        }
        Err(e) => {
            // E2EE transport error - both direct and relay failed.
            // Fall through to plaintext: if E2EE could not deliver at all
            // (peer unreachable or decryption failed on their side),
            // there is no duplicate risk.
            tracing::warn!("E2EE loan_request error, falling back to plaintext: {e}");
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    // SSRF validation: only needed here for direct HTTP to peer URL.
    // Relay-only peers (relay://) never reach this point because E2EE handles them.
    if let Err(e) = validate_url(&peer.url) {
        crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Cannot reach peer: {}", e) })),
        )
            .into_response();
    }
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            let resp_status = response.status();
            let body = response.text().await.unwrap_or_default();

            if resp_status.is_success() {
                // Parse response body to check for auto-acceptance
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("accepted")
                {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (plaintext)",
                        outgoing_id
                    );
                    let lender_request_id = parsed.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(db, &outgoing_id, &parsed, lender_request_id).await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Request sent", "status": "pending" })),
                )
                    .into_response()
            } else {
                // Parse lender response to check for auto-rejection
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("rejected")
                {
                    // Update outgoing request to rejected
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    let reason = parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (plaintext): {}",
                        outgoing_id,
                        reason
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": reason })),
                    )
                        .into_response();
                }
                crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => {
            crate::services::loan_service::mark_outgoing_request_failed(db, &outgoing_id).await;
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to contact peer" })),
            )
                .into_response()
        }
    }
}

pub async fn list_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::book;
    use crate::utils::cover_url::{self, ResolveScope};

    let requests = crate::models::p2p_outgoing_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Look up local books by ISBN so we can link to them (same pattern as list_requests)
    let isbns: Vec<String> = requests
        .iter()
        .map(|(req, _)| req.book_isbn.clone())
        .filter(|isbn| !isbn.is_empty())
        .collect();

    // Cover URLs must be servable by `CachedNetworkImage` on the UI. A raw
    // filesystem path in `books.cover_url` (typical of a local upload before
    // hub sync) would render a placeholder, so the rewrite to a hub URL or
    // `/api/books/{id}/cover` fallback happens here, keyed on the same scope
    // as `api/books.rs` list endpoint.
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

/// Delete all non-pending outgoing requests (cleanup).
pub async fn clear_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use sea_orm::ConnectionTrait;
    let result = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM p2p_outgoing_requests WHERE status != 'pending'".to_owned(),
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

/// Sync pending outgoing requests by querying each lender for current status.
///
/// For each pending outgoing request, sends a `request_status_query` via E2EE
/// (with relay fallback) to the lender. If the lender reports the request has been
/// accepted/rejected, updates the local outgoing request accordingly and creates
/// the borrowed copy if accepted.
pub async fn sync_outgoing_requests(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    let db = state.db();
    let pending = p2p_outgoing_request::Entity::find()
        .filter(p2p_outgoing_request::Column::Status.eq("pending"))
        .all(db)
        .await
        .unwrap_or_default();

    if pending.is_empty() {
        return (StatusCode::OK, Json(json!({ "synced": 0, "updated": 0 }))).into_response();
    }

    let mut synced = 0u32;
    let mut updated = 0u32;

    for outgoing in &pending {
        // Find the lender peer
        let lender = match peer::Entity::find_by_id(outgoing.to_peer_id).one(db).await {
            Ok(Some(p)) => p,
            _ => continue,
        };

        let query_payload = json!({
            "requester_request_id": outgoing.id,
        });

        // Try E2EE (with relay fallback)
        let result = try_send_e2ee(&state, &lender, "request_status_query", query_payload).await;
        synced += 1;

        if let Ok(Some(Some(ref clear_msg))) = result {
            let remote_status = clear_msg
                .payload
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");

            if remote_status != "pending" && remote_status != "not_found" {
                tracing::info!(
                    "Sync: outgoing request {} status changed to '{}'",
                    outgoing.id,
                    remote_status
                );

                if remote_status == "accepted" {
                    let lender_request_id =
                        clear_msg.payload.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(
                        db,
                        &outgoing.id,
                        &clear_msg.payload,
                        lender_request_id,
                    )
                    .await;
                } else {
                    // rejected or returned
                    let mut active: p2p_outgoing_request::ActiveModel = outgoing.clone().into();
                    active.status = Set(remote_status.to_string());
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    let _ = active.update(db).await;
                }
                updated += 1;
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "synced": synced, "updated": updated })),
    )
        .into_response()
}

pub async fn delete_outgoing_request(
    State(state): State<crate::infrastructure::AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;
    let db = state.db().clone();

    // 1. First, retrieve the request to get the peer info
    let request = match p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
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

    // 2. Get the peer URL to notify them
    let peer = match peer::Entity::find_by_id(request.to_peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::warn!(
                "Peer {} not found for outgoing request {}",
                request.to_peer_id,
                id
            );
            // Peer not found, just delete locally
            let _ = p2p_outgoing_request::Entity::delete_by_id(&id)
                .exec(&db)
                .await;
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 3. Notify the peer about the cancellation (best effort)
    let client = get_safe_client();
    let cancel_url = format!("{}/api/peers/requests/cancel/{}", peer.url, id);
    // Assert our own identity so the lender's ownership check accepts the cancel:
    // it resolves this uuid and requires it to name the borrower that made the request.
    let our_uuid = state.identity_service.library_uuid().map(|s| s.to_string());

    tracing::info!(
        "📡 Notifying peer {} of request cancellation: {}",
        peer.name,
        cancel_url
    );

    match client
        .delete(&cancel_url)
        .json(&json!({ "library_uuid": our_uuid }))
        .send()
        .await
    {
        Ok(res) => {
            if res.status().is_success() {
                tracing::info!("✅ Peer notified successfully");
            } else {
                tracing::warn!("⚠️ Peer notification returned: {}", res.status());
            }
        }
        Err(e) => {
            tracing::warn!("⚠️ Failed to notify peer (may be offline): {}", e);
            // Continue with local deletion anyway
        }
    }

    // 4. Delete locally
    match p2p_outgoing_request::Entity::delete_by_id(&id)
        .exec(&db)
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Receive cancellation notification from a peer who cancelled their outgoing request
pub async fn cancel_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
    // Optional so an older sender that posts no body still parses; a missing body then
    // reads as an anonymous cancel and is refused below.
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    tracing::info!("📨 Received cancellation notification for request: {}", id);

    // Ownership: a cancellation deletes an incoming request, so a guessed id would
    // otherwise let any host on the LAN drop any pending loan request. Only the borrower
    // that created the request may cancel it, mirroring the encrypted path's sender check.
    let request = match p2p_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        // Idempotent: nothing to cancel, and nothing to leak about who asked.
        Ok(None) => {
            tracing::warn!("Cancellation target not found: {}", id);
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            tracing::error!("❌ Failed to load request for cancellation: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let requester = peer::Entity::find_by_id(request.from_peer_id)
        .one(&db)
        .await
        .ok()
        .flatten();
    if requester
        .as_ref()
        .map(|p| p.key_exchange_done)
        .unwrap_or(false)
    {
        tracing::warn!(
            "Plaintext cancel for request {} names a key-exchanged peer; refusing",
            id
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "This request is served over the encrypted channel" })),
        )
            .into_response();
    }
    let claimed_uuid = body
        .as_ref()
        .and_then(|j| j.get("library_uuid"))
        .and_then(|v| v.as_str());
    if resolve_peer_by_library_uuid(&db, claimed_uuid)
        .await
        .map(|p| p.id)
        != Some(request.from_peer_id)
    {
        tracing::warn!(
            "Plaintext cancel for request {} carries no matching sender identity; refusing",
            id
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Sender does not own this request" })),
        )
            .into_response();
    }

    // Delete the incoming request that matches this ID
    match p2p_request::Entity::delete_by_id(&id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                tracing::warn!("Cancellation target not found: {}", id);
                // Return OK anyway - idempotent behavior
                StatusCode::OK.into_response()
            } else {
                tracing::info!("✅ Request {} cancelled successfully", id);
                StatusCode::OK.into_response()
            }
        }
        Err(e) => {
            tracing::error!("❌ Failed to cancel request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Receive status update notification from lender (updates local outgoing request)
pub async fn update_outgoing_status(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    let new_status = match payload.get("status").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing status field" })),
            )
                .into_response();
        }
    };

    tracing::info!(
        "📨 Received status update for outgoing request {}: {}",
        id,
        new_status
    );

    // Find the outgoing request
    let request = match p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            tracing::warn!("Outgoing request not found: {}", id);
            // Return OK anyway - idempotent
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Ownership: this endpoint has no authenticated sender, so a guessed request id
    // would otherwise let any host on the LAN drive someone else's loan and, on
    // "returned", purge the borrowed copy. Reconstruct the identity check that
    // `handle_status_update` (api/e2ee.rs) runs on the encrypted path: only the lender
    // the request names may move it.
    let lender = peer::Entity::find_by_id(request.to_peer_id)
        .one(&db)
        .await
        .ok()
        .flatten();
    // A lender that completed the key exchange would have used the encrypted,
    // authenticated channel, the only place a status update is trusted. A plaintext
    // update naming such a loan cannot be them, so refuse it.
    if lender
        .as_ref()
        .map(|p| p.key_exchange_done)
        .unwrap_or(false)
    {
        tracing::warn!(
            "Plaintext status update for request {} names a key-exchanged lender; refusing",
            id
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "This loan is served over the encrypted channel" })),
        )
            .into_response();
    }
    // For a keyless lender the only identity on offer is the `library_uuid` the payload
    // asserts. Resolve it (lookup only, never create) and require it to name that lender.
    // Absent or mismatched means anonymous: refuse rather than trust an unauthenticated POST.
    let claimed_uuid = payload.get("library_uuid").and_then(|v| v.as_str());
    if resolve_peer_by_library_uuid(&db, claimed_uuid)
        .await
        .map(|p| p.id)
        != Some(request.to_peer_id)
    {
        tracing::warn!(
            "Plaintext status update for request {} carries no matching sender identity; refusing",
            id
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Sender does not own this loan" })),
        )
            .into_response();
    }

    // Read what the cleanup below needs before the model is consumed by the update.
    // The request names the lender, the book it was accepted for, and its own title.
    let book_isbn = request.book_isbn.clone();
    let lender_peer_id = request.to_peer_id;
    let loan_book_id = request.book_id.clone();
    let request_title = request.book_title.clone();

    // Update the status
    let mut active: p2p_outgoing_request::ActiveModel = request.into();
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(_) => {
            tracing::info!("✅ Outgoing request {} updated to {}", id, new_status);

            // If the loan is returned, clean up the borrowed copy
            if new_status == "returned" {
                tracing::info!("🧹 Cleaning up borrowed copy for book ISBN: {}", book_isbn);

                // Resolve the book this loan names, declining rather than guessing when
                // only an ambiguous ISBN identifies it, then delete the copies this lender
                // lent. The row may also carry a contact loan, a copy the user owns, or
                // another peer's live loan; those are never touched.
                //
                // The book row itself stays, as it does in `retain_returned_book` and
                // `release_reclaimed_book`. A book read without being owned is a
                // first-class state carrying reading dates, a rating and notes the reader
                // entered, and this runs on an inbound message with nobody in front of the
                // screen. Removing a book from the library is left to an explicit user
                // action. `owned` is untouched for the same reason: the row may be a book
                // the user genuinely owns, reused by `create_borrowed_copy` on an ISBN
                // match.
                let resolved_book = crate::services::loan_service::resolve_returned_book(
                    &db,
                    loan_book_id.as_deref(),
                    &book_isbn,
                )
                .await;

                if let Some(book) = resolved_book.as_ref() {
                    crate::services::loan_service::purge_copies_lent_by(
                        &db,
                        &book.id,
                        lender_peer_id,
                    )
                    .await;
                }

                // Emit book_returned notification on borrower side. The request's own
                // title stands in when no local book row could be resolved.
                let book_title = resolved_book
                    .map(|b| b.title)
                    .unwrap_or_else(|| request_title.clone());
                let lender_name = peer::Entity::find_by_id(lender_peer_id)
                    .one(&db)
                    .await
                    .ok()
                    .flatten()
                    .map(|p| p.name)
                    .unwrap_or_default();
                crate::services::notification_service::emit(
                    &db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::BookReturned,
                        title: book_title,
                        body: Some(lender_name),
                        ref_type: Some("loan".to_string()),
                        ref_id: Some(id.clone()),
                    },
                )
                .await;
            }

            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::error!("❌ Failed to update outgoing request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct OutgoingLoanRequestDto {
    pub to_peer_url: String,
    pub book_isbn: String,
    pub book_title: String,
    pub request_id: Option<String>, // ID from remote peer for sync
}

pub async fn create_outgoing_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<OutgoingLoanRequestDto>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find Peer by URL, or auto-create if not found
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.to_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Auto-create peer from URL (will be updated on next mDNS discovery)
            tracing::info!(
                "📝 Auto-creating peer for outgoing request: {}",
                payload.to_peer_url
            );
            let new_peer = peer::ActiveModel {
                name: Set("Réseau local".to_string()), // Placeholder name
                url: Set(payload.to_peer_url.clone()),
                auto_approve: Set(false),
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

    // 2. Create Outgoing Request Log
    // Use request_id from remote peer if provided (for sync), otherwise generate new
    let request_id = payload
        .request_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_outgoing_request::ActiveModel {
        id: Set(request_id),
        to_peer_id: Set(peer.id),
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
            Json(json!({ "message": "Outgoing request logged" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save outgoing request: {}", e) })),
        )
            .into_response(),
    }
}
