#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
//! Copy Status State Machine Tests
//!
//! Covers: B2.1 Copy Status State Machine, B6.1 Full Sale Cycle (TNR)
//! Tests status transitions: loan restrictions, sale flow, cancel sale.

use rust_lib_app::db;
use rust_lib_app::models::copy::{self, Entity as Copy};
use rust_lib_app::models::loan::LoanDto;
use rust_lib_app::models::sale::SaleDto;
use rust_lib_app::services::loan_service;
use rust_lib_app::services::sale_service;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Create admin user + library + book + contact, return (library_id, book_id, contact_id)
async fn seed_test_data(db: &DatabaseConnection) -> (i32, i32, i32) {
    let now = chrono::Utc::now().to_rfc3339();

    // User
    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("admin".to_string()),
        password_hash: Set("hash".to_string()),
        role: Set("admin".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };
    let user = user.insert(db).await.unwrap();

    // Library
    let library = rust_lib_app::models::library::ActiveModel {
        name: Set("Test Library".to_string()),
        owner_id: Set(user.id),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };
    let library = library.insert(db).await.unwrap();

    // Book
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Test Book".to_string()),
        owned: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };
    let book = book.insert(db).await.unwrap();

    // Contact
    let contact = rust_lib_app::models::contact::ActiveModel {
        r#type: Set("person".to_string()),
        name: Set("Test Contact".to_string()),
        library_owner_id: Set(library.id),
        is_active: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let contact = contact.insert(db).await.unwrap();

    (library.id, book.id, contact.id)
}

/// Create a copy with the given status
async fn create_copy(db: &DatabaseConnection, book_id: i32, library_id: i32, status: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(library_id),
        status: Set(status.to_string()),
        is_temporary: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let result = copy.insert(db).await.unwrap();
    result.id
}

fn make_loan_dto(copy_id: i32, contact_id: i32, library_id: i32) -> LoanDto {
    LoanDto {
        id: None,
        copy_id,
        contact_id,
        library_id,
        loan_date: "2026-01-01".to_string(),
        due_date: "2026-02-01".to_string(),
        return_date: None,
        status: None,
        notes: None,
    }
}

fn make_sale_dto(copy_id: i32, library_id: i32) -> SaleDto {
    SaleDto {
        id: None,
        copy_id,
        contact_id: None,
        library_id,
        sale_date: "2026-01-15".to_string(),
        sale_price: 15.50,
        status: None,
        notes: None,
    }
}

// --- Loan restrictions ---

#[tokio::test]
async fn test_loan_available_copy_succeeds() {
    let db = setup_test_db().await;
    let (lib_id, book_id, contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "available").await;

    let result = loan_service::create_loan(&db, make_loan_dto(copy_id, contact_id, lib_id)).await;
    assert!(result.is_ok(), "Loan on available copy should succeed");

    // Verify copy status changed to "loaned"
    let copy = Copy::find_by_id(copy_id).one(&db).await.unwrap().unwrap();
    assert_eq!(copy.status, "loaned");
}

#[tokio::test]
async fn test_loan_borrowed_copy_rejected() {
    let db = setup_test_db().await;
    let (lib_id, book_id, contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "borrowed").await;

    let result = loan_service::create_loan(&db, make_loan_dto(copy_id, contact_id, lib_id)).await;
    assert!(result.is_err(), "Loan on borrowed copy must be rejected");

    match result.unwrap_err() {
        loan_service::ServiceError::InvalidState(msg) => {
            assert!(msg.contains("borrowed"), "Error should mention 'borrowed'");
        }
        other => panic!("Expected InvalidState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_loan_lost_copy_rejected() {
    let db = setup_test_db().await;
    let (lib_id, book_id, contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "lost").await;

    let result = loan_service::create_loan(&db, make_loan_dto(copy_id, contact_id, lib_id)).await;
    assert!(result.is_err(), "Loan on lost copy must be rejected");

    match result.unwrap_err() {
        loan_service::ServiceError::InvalidState(msg) => {
            assert!(msg.contains("lost"), "Error should mention 'lost'");
        }
        other => panic!("Expected InvalidState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_loan_nonexistent_copy_returns_not_found() {
    let db = setup_test_db().await;
    let (lib_id, _book_id, contact_id) = seed_test_data(&db).await;

    let result = loan_service::create_loan(&db, make_loan_dto(9999, contact_id, lib_id)).await;
    assert!(result.is_err());

    match result.unwrap_err() {
        loan_service::ServiceError::NotFound => {} // Expected
        other => panic!("Expected NotFound, got {:?}", other),
    }
}

// --- Loan return ---

#[tokio::test]
async fn test_return_loan_restores_available_status() {
    let db = setup_test_db().await;
    let (lib_id, book_id, contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "available").await;

    // Create loan
    let loan = loan_service::create_loan(&db, make_loan_dto(copy_id, contact_id, lib_id))
        .await
        .unwrap();

    // Return loan
    let returned = loan_service::return_loan(&db, loan.id).await.unwrap();
    assert_eq!(returned.status, "returned");

    // Copy is available again
    let copy = Copy::find_by_id(copy_id).one(&db).await.unwrap().unwrap();
    assert_eq!(copy.status, "available");
}

#[tokio::test]
async fn test_return_already_returned_loan_rejected() {
    let db = setup_test_db().await;
    let (lib_id, book_id, contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "available").await;

    let loan = loan_service::create_loan(&db, make_loan_dto(copy_id, contact_id, lib_id))
        .await
        .unwrap();
    loan_service::return_loan(&db, loan.id).await.unwrap();

    // Try to return again
    let result = loan_service::return_loan(&db, loan.id).await;
    assert!(result.is_err(), "Double return must be rejected");

    match result.unwrap_err() {
        loan_service::ServiceError::InvalidState(msg) => {
            assert!(msg.contains("already returned"));
        }
        other => panic!("Expected InvalidState, got {:?}", other),
    }
}

// --- Sale flow ---

#[tokio::test]
async fn test_sale_changes_copy_to_sold() {
    let db = setup_test_db().await;
    let (lib_id, book_id, _contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "available").await;

    let sale = sale_service::record_sale(&db, make_sale_dto(copy_id, lib_id))
        .await
        .unwrap();

    assert_eq!(sale.status, "completed");
    assert_eq!(sale.sale_price, 15.50);

    // Copy status is now "sold"
    let copy = Copy::find_by_id(copy_id).one(&db).await.unwrap().unwrap();
    assert_eq!(copy.status, "sold");
    assert!(copy.sold_at.is_some(), "sold_at must be set after a sale");
}

#[tokio::test]
async fn test_cancel_sale_restores_available() {
    let db = setup_test_db().await;
    let (lib_id, book_id, _contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "available").await;

    // Record sale
    let sale = sale_service::record_sale(&db, make_sale_dto(copy_id, lib_id))
        .await
        .unwrap();

    // Cancel sale
    let cancelled = sale_service::cancel_sale(&db, sale.id).await.unwrap();
    assert_eq!(cancelled.status, "cancelled");

    // Copy is available again
    let copy = Copy::find_by_id(copy_id).one(&db).await.unwrap().unwrap();
    assert_eq!(copy.status, "available");
    assert!(
        copy.sold_at.is_none(),
        "sold_at must be cleared after cancel"
    );
}

#[tokio::test]
async fn test_cancel_already_cancelled_sale_rejected() {
    let db = setup_test_db().await;
    let (lib_id, book_id, _contact_id) = seed_test_data(&db).await;
    let copy_id = create_copy(&db, book_id, lib_id, "available").await;

    let sale = sale_service::record_sale(&db, make_sale_dto(copy_id, lib_id))
        .await
        .unwrap();
    sale_service::cancel_sale(&db, sale.id).await.unwrap();

    // Try to cancel again
    let result = sale_service::cancel_sale(&db, sale.id).await;
    assert!(
        result.is_err(),
        "Cancelling an already-cancelled sale must be rejected"
    );

    match result.unwrap_err() {
        sale_service::ServiceError::InvalidState(msg) => {
            assert!(msg.contains("already cancelled"));
        }
        other => panic!("Expected InvalidState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_sale_nonexistent_copy_returns_not_found() {
    let db = setup_test_db().await;
    let (lib_id, _book_id, _contact_id) = seed_test_data(&db).await;

    let result = sale_service::record_sale(&db, make_sale_dto(9999, lib_id)).await;
    assert!(result.is_err());

    match result.unwrap_err() {
        sale_service::ServiceError::NotFound => {} // Expected
        other => panic!("Expected NotFound, got {:?}", other),
    }
}
