use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use chrono::Local;

use crate::models::loan::{self, Entity as Loan};
use crate::models::copy::{self, Entity as Copy};

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

    let loans = Loan::find()
        .filter(condition)
        .order_by_desc(loan::Column::LoanDate)
        .all(&db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "loans": loans })))
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
        return Err((StatusCode::BAD_REQUEST, format!("Copy is currently {}", copy.status)));
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
    copy_active.update(&db).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "loan": saved_loan, "message": "Loan created successfully" })))
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
        return Err((StatusCode::BAD_REQUEST, "Loan is already returned".to_string()));
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
        .ok_or((StatusCode::NOT_FOUND, "Associated copy not found".to_string()))?;

    let mut copy_active: copy::ActiveModel = copy.into();
    copy_active.status = Set("available".to_owned());
    copy_active.update(&db).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "loan": updated_loan, "message": "Loan returned successfully" })))
}
