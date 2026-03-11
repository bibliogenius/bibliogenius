//! Loan Service - Pure business logic without HTTP layer

use chrono::Local;
use sea_orm::*;
use std::collections::HashMap;

use crate::models::book::Entity as Book;
use crate::models::contact::Entity as Contact;
use crate::models::copy::{self, Entity as Copy};
use crate::models::loan::{self, Entity as Loan, LoanDto};
use crate::models::p2p_outgoing_request::{self, Entity as P2pOutgoingRequest};
use crate::models::p2p_request::{self, Entity as P2pRequest};

/// Error type for service operations
#[derive(Debug)]
pub enum ServiceError {
    Database(String),
    NotFound,
    InvalidState(String),
}

impl From<sea_orm::DbErr> for ServiceError {
    fn from(e: sea_orm::DbErr) -> Self {
        ServiceError::Database(e.to_string())
    }
}

/// Enriched loan with related data
#[derive(Debug, Clone)]
pub struct LoanWithDetails {
    pub id: i32,
    pub copy_id: i32,
    pub contact_id: i32,
    pub library_id: i32,
    pub loan_date: String,
    pub due_date: String,
    pub return_date: Option<String>,
    pub status: String,
    pub notes: Option<String>,
    pub contact_name: String,
    pub book_title: String,
    pub book_id: Option<i32>,
    pub cover_url: Option<String>,
    pub isbn: Option<String>,
}

/// Filter parameters for listing loans
#[derive(Debug, Default, Clone)]
pub struct LoanFilter {
    pub library_id: Option<i32>,
    pub status: Option<String>,
    pub contact_id: Option<i32>,
}

/// List all loans with related contact and book info
pub async fn list_loans(
    db: &DatabaseConnection,
    filter: LoanFilter,
) -> Result<Vec<LoanWithDetails>, ServiceError> {
    let mut condition = Condition::all();

    if let Some(library_id) = filter.library_id {
        condition = condition.add(loan::Column::LibraryId.eq(library_id));
    }

    if let Some(status) = filter.status {
        condition = condition.add(loan::Column::Status.eq(status));
    }

    if let Some(contact_id) = filter.contact_id {
        condition = condition.add(loan::Column::ContactId.eq(contact_id));
    }

    let loans_with_contacts = Loan::find()
        .filter(condition)
        .order_by_desc(loan::Column::LoanDate)
        .find_also_related(Contact)
        .all(db)
        .await?;

    // Collect copy IDs to fetch books
    let copy_ids: Vec<i32> = loans_with_contacts.iter().map(|(l, _)| l.copy_id).collect();

    // Fetch copies with books (title, id, cover_url, isbn)
    let mut copy_book_map: HashMap<i32, (String, i32, Option<String>, Option<String>)> =
        HashMap::new();

    if !copy_ids.is_empty() {
        let copies_with_books = Copy::find()
            .filter(copy::Column::Id.is_in(copy_ids))
            .find_also_related(Book)
            .all(db)
            .await?;

        for (copy, book) in copies_with_books {
            if let Some(book) = book {
                copy_book_map.insert(copy.id, (book.title, book.id, book.cover_url, book.isbn));
            }
        }
    }

    let result: Vec<LoanWithDetails> = loans_with_contacts
        .into_iter()
        .map(|(loan, contact)| {
            let contact_name = contact
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Unknown".to_string());
            let book_info = copy_book_map.get(&loan.copy_id);
            let book_title = book_info
                .map(|(title, _, _, _)| title.clone())
                .unwrap_or_else(|| "Unknown".to_string());
            let book_id = book_info.map(|(_, id, _, _)| *id);
            let cover_url = book_info.and_then(|(_, _, url, _)| url.clone());
            let isbn = book_info.and_then(|(_, _, _, isbn)| isbn.clone());

            LoanWithDetails {
                id: loan.id,
                copy_id: loan.copy_id,
                contact_id: loan.contact_id,
                library_id: loan.library_id,
                loan_date: loan.loan_date,
                due_date: loan.due_date,
                return_date: loan.return_date,
                status: loan.status,
                notes: loan.notes,
                contact_name,
                book_title,
                book_id,
                cover_url,
                isbn,
            }
        })
        .collect();

    Ok(result)
}

/// Create a new loan
pub async fn create_loan(
    db: &DatabaseConnection,
    dto: LoanDto,
) -> Result<loan::Model, ServiceError> {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Check if copy exists and is available
    let copy = Copy::find_by_id(dto.copy_id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    if copy.status == "borrowed" || copy.status == "lost" {
        return Err(ServiceError::InvalidState(format!(
            "Copy is currently {}",
            copy.status
        )));
    }

    // 2. Create Loan
    let new_loan = loan::ActiveModel {
        copy_id: Set(dto.copy_id),
        contact_id: Set(dto.contact_id),
        library_id: Set(dto.library_id),
        loan_date: Set(dto.loan_date),
        due_date: Set(dto.due_date),
        return_date: Set(None),
        status: Set("active".to_owned()),
        notes: Set(dto.notes),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let saved_loan = new_loan.insert(db).await?;

    let _ = crate::sync::log_operation(
        db,
        "loan",
        saved_loan.id,
        "INSERT",
        Some(serde_json::json!({ "copy_id": saved_loan.copy_id })),
    )
    .await;

    // 3. Update Copy status to 'loaned'
    let mut copy_active: copy::ActiveModel = copy.into();
    copy_active.status = Set("loaned".to_owned());
    copy_active.update(db).await?;

    let _ = crate::sync::log_operation(
        db,
        "copy",
        dto.copy_id,
        "UPDATE",
        Some(serde_json::json!({ "status": "loaned" })),
    )
    .await;

    Ok(saved_loan)
}

/// Return a loan
pub async fn return_loan(db: &DatabaseConnection, id: i32) -> Result<loan::Model, ServiceError> {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Find Loan
    let loan = Loan::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    if loan.status == "returned" {
        return Err(ServiceError::InvalidState(
            "Loan is already returned".to_string(),
        ));
    }

    // 2. Update Loan
    let mut loan_active: loan::ActiveModel = loan.clone().into();
    loan_active.return_date = Set(Some(now.clone()));
    loan_active.status = Set("returned".to_owned());
    loan_active.updated_at = Set(now);

    let updated_loan = loan_active.update(db).await?;

    let _ = crate::sync::log_operation(
        db,
        "loan",
        updated_loan.id,
        "UPDATE",
        Some(serde_json::json!({ "status": "returned" })),
    )
    .await;

    // 3. Update Copy status to 'available'
    let copy = Copy::find_by_id(loan.copy_id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    let mut copy_active: copy::ActiveModel = copy.into();
    copy_active.status = Set("available".to_owned());
    copy_active.update(db).await?;

    let _ = crate::sync::log_operation(
        db,
        "copy",
        loan.copy_id,
        "UPDATE",
        Some(serde_json::json!({ "status": "available" })),
    )
    .await;

    Ok(updated_loan)
}

/// Count total loans
pub async fn count_loans(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = Loan::find().count(db).await?;
    Ok(count as i64)
}

/// Count active loans
pub async fn count_active_loans(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = Loan::find()
        .filter(loan::Column::Status.eq("active"))
        .count(db)
        .await?;
    Ok(count as i64)
}

/// Count returned loans
pub async fn count_returned_loans(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = Loan::find()
        .filter(loan::Column::Status.eq("returned"))
        .count(db)
        .await?;
    Ok(count as i64)
}

/// Delete all returned loans, returns the number of deleted rows
pub async fn delete_returned_loans(db: &DatabaseConnection) -> Result<u64, ServiceError> {
    let result = Loan::delete_many()
        .filter(loan::Column::Status.eq("returned"))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}

/// Count closed incoming P2P requests (not pending)
pub async fn count_closed_incoming_requests(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = P2pRequest::find()
        .filter(p2p_request::Column::Status.ne("pending"))
        .count(db)
        .await?;
    Ok(count as i64)
}

/// Delete all closed incoming P2P requests (not pending)
pub async fn delete_closed_incoming_requests(db: &DatabaseConnection) -> Result<u64, ServiceError> {
    let result = P2pRequest::delete_many()
        .filter(p2p_request::Column::Status.ne("pending"))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}

/// Count closed outgoing P2P requests (not pending)
pub async fn count_closed_outgoing_requests(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = P2pOutgoingRequest::find()
        .filter(p2p_outgoing_request::Column::Status.ne("pending"))
        .count(db)
        .await?;
    Ok(count as i64)
}

/// Delete all closed outgoing P2P requests (not pending)
pub async fn delete_closed_outgoing_requests(db: &DatabaseConnection) -> Result<u64, ServiceError> {
    let result = P2pOutgoingRequest::delete_many()
        .filter(p2p_outgoing_request::Column::Status.ne("pending"))
        .exec(db)
        .await?;
    Ok(result.rows_affected)
}
