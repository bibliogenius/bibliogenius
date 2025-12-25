use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use rust_lib_app::api;
use rust_lib_app::auth;
use rust_lib_app::db;
use sea_orm::DatabaseConnection;
use tower::util::ServiceExt; // for `oneshot`

// Helper to create a test database
async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

// Helper to create a valid auth token
fn get_test_token() -> String {
    auth::create_jwt("test_user", "admin").expect("Failed to create token")
}

#[tokio::test]
async fn test_get_book_not_found() {
    let db = setup_test_db().await;
    let token = get_test_token();

    // Setup Router
    let app = Router::new()
        .route("/books/:id", axum::routing::get(api::books::get_book))
        .route("/books/:id", axum::routing::put(api::books::update_book))
        .with_state(db);

    // Test GET Non-Existent Book
    let req = Request::builder()
        .uri("/books/999")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Test Update Non-Existent Book
    let payload = serde_json::json!({
        "title": "Non-existent Book"
    });

    let req = Request::builder()
        .uri("/books/999")
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_book_idempotency() {
    let db = setup_test_db().await;
    let token = get_test_token();

    // Setup Router
    let app = Router::new()
        .route("/books/:id", axum::routing::delete(api::books::delete_book))
        .with_state(db);

    // Test Delete Non-Existent Book (Should be 200 OK)
    let req = Request::builder()
        .uri("/books/999")
        .method("DELETE")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_create_book_invalid_input() {
    let db = setup_test_db().await;
    let token = get_test_token();

    // Setup Router
    let app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(db);

    // Test Invalid JSON
    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("invalid json"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    // Axum's Json extractor returns 400 for malformed JSON
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
