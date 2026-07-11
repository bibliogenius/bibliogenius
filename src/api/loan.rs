use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::Local;
use sea_orm::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::infrastructure::AppState;
use crate::models::book::Entity as Book;
use crate::models::contact::Entity as Contact;
use crate::models::copy::{self, Entity as Copy};
use crate::models::loan::{self, Entity as Loan};

#[derive(Deserialize)]
pub struct ListLoansQuery {
    pub library_id: Option<i32>,
    pub status: Option<String>,
    pub contact_id: Option<i32>,
}

pub async fn list_loans(
    State(db): State<DatabaseConnection>,
    Query(query): Query<ListLoansQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut condition = Condition::all();

    if let Some(library_id) = query.library_id {
        condition = condition.add(loan::Column::LibraryId.eq(library_id));
    }

    if let Some(status) = query.status {
        condition = condition.add(loan::Column::Status.eq(status));
    }

    if let Some(contact_id) = query.contact_id {
        condition = condition.add(loan::Column::ContactId.eq(contact_id));
    }

    let loans_with_contacts = Loan::find()
        .filter(condition)
        .order_by_desc(loan::Column::LoanDate)
        .find_also_related(Contact)
        .all(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Collect copy IDs to fetch books
    let copy_ids: Vec<String> = loans_with_contacts
        .iter()
        .map(|(l, _)| l.copy_id.clone())
        .collect();

    // Fetch copies with books
    // We only need to fetch if there are loans
    let mut copy_book_map = std::collections::HashMap::new();

    if !copy_ids.is_empty() {
        let copies_with_books = Copy::find()
            .filter(copy::Column::Id.is_in(copy_ids))
            .find_also_related(Book)
            .all(&db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        for (copy, book) in copies_with_books {
            if let Some(book) = book {
                copy_book_map.insert(copy.id, book);
            }
        }
    }

    let result: Vec<Value> = loans_with_contacts
        .into_iter()
        .map(|(loan, contact)| {
            let book = copy_book_map.get(&loan.copy_id);
            let contact_name = contact
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or("Unknown".to_string());
            let book_title = book
                .as_ref()
                .map(|b| b.title.clone())
                .unwrap_or("Unknown".to_string());

            let book_id = book.as_ref().map(|b| b.id.clone());
            let cover_url = book.as_ref().and_then(|b| b.cover_url.clone());
            let isbn = book.as_ref().and_then(|b| b.isbn.clone());

            json!({
                "id": loan.id,
                "copy_id": loan.copy_id,
                "contact_id": loan.contact_id,
                "library_id": loan.library_id,
                "loan_date": loan.loan_date,
                "due_date": loan.due_date,
                "return_date": loan.return_date,
                "status": loan.status,
                "notes": loan.notes,
                "contact_name": contact_name,
                "book_title": book_title,
                "book_id": book_id,
                "cover_url": cover_url,
                "isbn": isbn,
                "contact": contact.map(|c| json!({"name": c.name})),
                "book": book.map(|b| json!({"title": b.title})),
            })
        })
        .collect();

    Ok(Json(json!({ "loans": result })))
}

pub async fn create_loan(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<loan::LoanDto>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Check if copy exists and is available
    let copy = Copy::find_by_id(payload.copy_id.clone())
        .one(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Copy not found".to_string()))?;

    if copy.status != "available" {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Copy is currently {}", copy.status),
        ));
    }

    // 2. Create Loan
    let new_loan = loan::ActiveModel {
        copy_id: Set(payload.copy_id),
        contact_id: Set(payload.contact_id),
        library_id: Set(payload.library_id),
        loan_date: Set(payload.loan_date),
        due_date: Set(payload.due_date),
        return_date: Set(None),
        status: Set("active".to_owned()),
        notes: Set(payload.notes),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let saved_loan = new_loan
        .insert(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 3. Update Copy status to 'loaned'
    let mut copy_active: copy::ActiveModel = copy.into();
    copy_active.status = Set("loaned".to_owned());
    copy_active
        .update(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        json!({ "loan": saved_loan, "message": "Loan created successfully" }),
    ))
}

pub async fn return_loan(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let db = state.db().clone();
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Find Loan
    let loan = Loan::find_by_id(id.clone())
        .one(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Loan not found".to_string()))?;

    if loan.status == "returned" {
        return Err((
            StatusCode::BAD_REQUEST,
            "Loan is already returned".to_string(),
        ));
    }

    // 2. Update Loan
    let mut loan_active: loan::ActiveModel = loan.clone().into();
    loan_active.return_date = Set(Some(now.clone()));
    loan_active.status = Set("returned".to_owned());
    loan_active.updated_at = Set(now.clone());

    let updated_loan = loan_active
        .update(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 3. Update Copy status to 'available'
    // First fetch the copy to get its full state
    let copy = Copy::find_by_id(loan.copy_id.clone())
        .one(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Associated copy not found".to_string(),
        ))?;

    let mut copy_active: copy::ActiveModel = copy.clone().into();
    copy_active.status = Set("available".to_owned());
    copy_active
        .update(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 4. Emit book_returned notification
    if let Ok(Some(book)) = Book::find_by_id(copy.book_id.clone()).one(&db).await {
        let contact_name = Contact::find_by_id(loan.contact_id.clone())
            .one(&db)
            .await
            .ok()
            .flatten()
            .map(|c| c.name)
            .unwrap_or_default();
        crate::services::notification_service::emit(
            &db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BookReturned,
                title: book.title,
                body: Some(contact_name),
                ref_type: Some("loan".to_string()),
                ref_id: Some(id.to_string()),
            },
        )
        .await;
    }

    // 5. Check for P2P implications
    // If the contact is a "Library" (Peer), we should notify them and update the request
    use crate::models::{book, contact, p2p_request, peer};

    // Find contact
    if let Some(contact) = contact::Entity::find_by_id(loan.contact_id)
        .one(&db)
        .await
        .unwrap_or(None)
        && contact.r#type == "Library"
    {
        tracing::info!(
            "🔄 Return loan associated with Peer Contact: {}",
            contact.name
        );

        // Find Peer by name
        if let Some(peer) = peer::Entity::find()
            .filter(peer::Column::Name.eq(&contact.name))
            .one(&db)
            .await
            .unwrap_or(None)
        {
            // Find associated book to get ISBN
            if let Some(book) = book::Entity::find_by_id(copy.book_id)
                .one(&db)
                .await
                .unwrap_or(None)
                && let Some(isbn) = &book.isbn
            {
                // Find the latest 'accepted' request for this book from this peer
                if let Some(request) = p2p_request::Entity::find()
                    .filter(p2p_request::Column::BookIsbn.eq(isbn))
                    .filter(p2p_request::Column::FromPeerId.eq(peer.id))
                    .filter(p2p_request::Column::Status.eq("accepted"))
                    .order_by_desc(p2p_request::Column::CreatedAt)
                    .one(&db)
                    .await
                    .unwrap_or(None)
                {
                    tracing::info!("found p2p request to return: {}", request.id);

                    // Update local request status
                    let mut req_active: p2p_request::ActiveModel = request.clone().into();
                    req_active.status = Set("returned".to_owned());
                    req_active.updated_at = Set(now.clone());
                    let _ = req_active.update(&db).await;

                    // Notify the borrower that the loan is returned. Encrypted channel
                    // first; fall back to plaintext only for a peer without keys, and then
                    // assert our identity so the borrower's ownership check accepts it (it
                    // resolves this uuid and requires it to name the lender). This path used
                    // to POST plaintext unconditionally, which both leaked to E2EE peers and,
                    // now that the borrower authenticates the sender, would be refused.
                    //
                    // Spawned and fire-and-forget, as this notification always was: the return
                    // has already succeeded locally, and `try_send_e2ee` may wait up to the
                    // relay timeout, which must never block the HTTP response.
                    let our_uuid = state.identity_service.library_uuid().map(|s| s.to_string());
                    let req_id = request.id.clone();
                    let state = state.clone();
                    let notify_peer = peer.clone();
                    tokio::spawn(async move {
                        match crate::api::peer::try_send_e2ee(
                            &state,
                            &notify_peer,
                            "status_update",
                            serde_json::json!({ "loan_id": req_id.clone(), "status": "returned" }),
                        )
                        .await
                        {
                            Ok(Some(_)) => {
                                tracing::info!("✅ Borrower notified of return (encrypted)");
                            }
                            Err(e) => {
                                // An E2EE channel that errors is not one that is absent: do
                                // not retry in plaintext, to avoid a duplicate notification.
                                tracing::warn!(
                                    "Return notification error (no plaintext fallback): {e}"
                                );
                            }
                            Ok(None) => {
                                let url = format!(
                                    "{}/api/peers/requests/status/{}",
                                    notify_peer.url, req_id
                                );
                                tracing::info!("📡 Notifying peer of return: PUT {}", url);

                                // SSRF-safe client (bounded timeout, no redirects), matching
                                // the twin plaintext fallback in `update_request_status`.
                                match crate::api::peer::get_safe_client()
                                    .put(&url)
                                    .json(&serde_json::json!({
                                        "status": "returned",
                                        "library_uuid": our_uuid,
                                    }))
                                    .send()
                                    .await
                                {
                                    Ok(res) if res.status().is_success() => {
                                        tracing::info!("✅ Peer notified of return successfully");
                                    }
                                    Ok(res) => {
                                        tracing::warn!(
                                            "⚠️ Peer notification failed: status {}",
                                            res.status()
                                        );
                                    }
                                    Err(e) => {
                                        tracing::error!("❌ Failed to notify peer: {}", e);
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    }

    Ok(Json(json!({
        "loan": updated_loan,
        "message": "Loan returned successfully",
        "p2p_notified": true
    })))
}

// ── Loan Settings (Clean Architecture) ──────────────────────────────

#[derive(Deserialize)]
pub struct UpdateLoanSettingsPayload {
    pub default_loan_duration_days: i32,
    pub per_book_duration_enabled: bool,
    #[serde(default = "default_reminder_days")]
    pub reminder_days_before_due: i32,
}

fn default_reminder_days() -> i32 {
    2
}

pub async fn get_loan_settings(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let settings = state.loan_settings_repo.get_settings().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;

    Ok(Json(json!({
        "default_loan_duration_days": settings.default_loan_duration_days,
        "per_book_duration_enabled": settings.per_book_duration_enabled,
        "reminder_days_before_due": settings.reminder_days_before_due,
    })))
}

pub async fn update_loan_settings(
    State(state): State<AppState>,
    Json(payload): Json<UpdateLoanSettingsPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    use crate::domain::LoanSettings;

    let updated = state
        .loan_settings_repo
        .update_settings(LoanSettings {
            default_loan_duration_days: payload.default_loan_duration_days,
            per_book_duration_enabled: payload.per_book_duration_enabled,
            reminder_days_before_due: payload.reminder_days_before_due,
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;

    Ok(Json(json!({
        "default_loan_duration_days": updated.default_loan_duration_days,
        "per_book_duration_enabled": updated.per_book_duration_enabled,
        "reminder_days_before_due": updated.reminder_days_before_due,
    })))
}

pub async fn get_effective_loan_duration(
    State(state): State<AppState>,
    Path(book_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let days = state
        .loan_settings_repo
        .get_effective_duration(&book_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;

    Ok(Json(json!({ "duration_days": days })))
}
