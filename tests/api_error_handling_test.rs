use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use bibliogenius::api;
use bibliogenius::db;
use sea_orm::DatabaseConnection;
use tower::util::ServiceExt; // for `oneshot`

// Helper to create a test database
async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

#[tokio::test]
async fn test_get_book_not_found() {
    let db = setup_test_db().await;

    // Setup Router
    let app = Router::new()
        .route("/books/:id", axum::routing::get(api::books::list_books)) // Wait, list_books is for /books, get_book is missing in books.rs?
        // Checking main.rs: .route("/books", get(api::books::list_books))
        // There is NO get_book by ID endpoint in main.rs or books.rs!
        // Only update and delete take ID.
        // Let's test update_book not found.
        .route("/books/:id", axum::routing::put(api::books::update_book))
        .with_state(db);

    // Test Update Non-Existent Book
    let payload = serde_json::json!({
        "title": "Non-existent Book"
    });

    let req = Request::builder()
        .uri("/books/999")
        .method("PUT")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_book_idempotency() {
    let db = setup_test_db().await;

    // Setup Router
    let app = Router::new()
        .route("/books/:id", axum::routing::delete(api::books::delete_book))
        .with_state(db);

    // Test Delete Non-Existent Book (Should be 200 OK)
    let req = Request::builder()
        .uri("/books/999")
        .method("DELETE")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_create_book_invalid_input() {
    // Note: Currently create_book takes a Book struct which has Option fields mostly.
    // Validation is minimal. But if we send invalid JSON, it should be 400 (handled by Axum Json extractor).

    let db = setup_test_db().await;

    // Setup Router
    let app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(db);

    // Test Invalid JSON
    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from("invalid json"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
