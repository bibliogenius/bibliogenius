#![allow(clippy::needless_update)]
//! HTTP-level tests for the loan endpoints (POST /loans, PUT /loans/:id/return).
//!
//! These validate the API handler path (loan.rs), which is distinct from the
//! service-layer tests in copy_status_test.rs that test loan_service.rs.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post, put},
};
use rust_lib_app::api::loan;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::copy::{self, Entity as Copy};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::json;
use tower::util::ServiceExt;

async fn setup() -> (AppState, i32, i32, i32) {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    let state = AppState::new(db.clone());

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
    let user = user.insert(&db).await.unwrap();

    // Library
    let library = rust_lib_app::models::library::ActiveModel {
        name: Set("Lib".to_string()),
        owner_id: Set(user.id),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };
    let library = library.insert(&db).await.unwrap();

    // Book
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Test Book".to_string()),
        owned: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };
    let book = book.insert(&db).await.unwrap();

    // Contact
    let contact = rust_lib_app::models::contact::ActiveModel {
        r#type: Set("person".to_string()),
        name: Set("Alice".to_string()),
        library_owner_id: Set(library.id),
        is_active: Set(true),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let contact = contact.insert(&db).await.unwrap();

    (state, library.id, book.id, contact.id)
}

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
    copy.insert(db).await.unwrap().id
}

fn loan_router() -> Router<AppState> {
    Router::new()
        .route("/loans", get(loan::list_loans).post(loan::create_loan))
        .route("/loans/:id/return", put(loan::return_loan))
}

fn loan_body(copy_id: i32, contact_id: i32, library_id: i32) -> String {
    json!({
        "copy_id": copy_id,
        "contact_id": contact_id,
        "library_id": library_id,
        "loan_date": "2026-04-11",
        "due_date": "2026-05-11"
    })
    .to_string()
}

// -- Tests --

#[tokio::test]
async fn test_http_create_loan_sets_copy_status_to_loaned() {
    let (state, lib_id, book_id, contact_id) = setup().await;
    let copy_id = create_copy(state.db(), book_id, lib_id, "available").await;

    let app = loan_router().with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/loans")
        .header("content-type", "application/json")
        .body(Body::from(loan_body(copy_id, contact_id, lib_id)))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify copy status is "loaned", not "borrowed"
    let copy = Copy::find_by_id(copy_id)
        .one(state.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.status, "loaned");
}

#[tokio::test]
async fn test_http_create_loan_on_loaned_copy_returns_400() {
    let (state, lib_id, book_id, contact_id) = setup().await;
    let copy_id = create_copy(state.db(), book_id, lib_id, "loaned").await;

    let app = loan_router().with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/loans")
        .header("content-type", "application/json")
        .body(Body::from(loan_body(copy_id, contact_id, lib_id)))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_http_create_loan_on_sold_copy_returns_400() {
    let (state, lib_id, book_id, contact_id) = setup().await;
    let copy_id = create_copy(state.db(), book_id, lib_id, "sold").await;

    let app = loan_router().with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/loans")
        .header("content-type", "application/json")
        .body(Body::from(loan_body(copy_id, contact_id, lib_id)))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_http_create_loan_on_lost_copy_returns_400() {
    let (state, lib_id, book_id, contact_id) = setup().await;
    let copy_id = create_copy(state.db(), book_id, lib_id, "lost").await;

    let app = loan_router().with_state(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri("/loans")
        .header("content-type", "application/json")
        .body(Body::from(loan_body(copy_id, contact_id, lib_id)))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_http_return_loan_restores_available() {
    let (state, lib_id, book_id, contact_id) = setup().await;
    let copy_id = create_copy(state.db(), book_id, lib_id, "available").await;

    let app = loan_router().with_state(state.clone());

    // Create loan
    let req = Request::builder()
        .method("POST")
        .uri("/loans")
        .header("content-type", "application/json")
        .body(Body::from(loan_body(copy_id, contact_id, lib_id)))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let loan_id = json["loan"]["id"].as_i64().unwrap();

    // Return loan
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/loans/{loan_id}/return"))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Copy is available again
    let copy = Copy::find_by_id(copy_id)
        .one(state.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.status, "available");
}
