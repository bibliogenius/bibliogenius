use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Local;
use sea_orm::*;
use serde::Deserialize;
use serde_json::{json, Value};

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
    let copy_ids: Vec<i32> = loans_with_contacts.iter().map(|(l, _)| l.copy_id).collect();

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
    let copy = Copy::find_by_id(payload.copy_id)
        .one(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Copy not found".to_string()))?;

    if copy.status == "borrowed" || copy.status == "lost" {
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

    // 3. Update Copy status to 'borrowed'
    let mut copy_active: copy::ActiveModel = copy.into();
    copy_active.status = Set("borrowed".to_owned());
    copy_active
        .update(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        json!({ "loan": saved_loan, "message": "Loan created successfully" }),
    ))
}

pub async fn return_loan(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Find Loan
    let loan = Loan::find_by_id(id)
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
    loan_active.updated_at = Set(now);

    let updated_loan = loan_active
        .update(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 3. Update Copy status to 'available'
    // First fetch the copy to get its full state
    let copy = Copy::find_by_id(loan.copy_id)
        .one(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Associated copy not found".to_string(),
        ))?;

    let mut copy_active: copy::ActiveModel = copy.into();
    copy_active.status = Set("available".to_owned());
    copy_active
        .update(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(
        json!({ "loan": updated_loan, "message": "Loan returned successfully" }),
    ))
}
