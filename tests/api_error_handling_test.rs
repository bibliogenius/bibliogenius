use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use rust_lib_app::api;
use rust_lib_app::auth;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use tower::util::ServiceExt; // for `oneshot`

// Helper to create a test app state
async fn setup_test_state() -> AppState {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    AppState::new(db)
}

// Helper to create a valid auth token
fn get_test_token() -> String {
    auth::create_jwt("test_user", "admin").expect("Failed to create token")
}

#[tokio::test]
async fn test_get_book_not_found() {
    let state = setup_test_state().await;
    let token = get_test_token();

    // Setup Router
    let app = Router::new()
        .route("/books/:id", axum::routing::get(api::books::get_book))
        .route("/books/:id", axum::routing::put(api::books::update_book))
        .with_state(state);

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
    let state = setup_test_state().await;
    let token = get_test_token();

    // Setup Router
    let app = Router::new()
        .route("/books/:id", axum::routing::delete(api::books::delete_book))
        .with_state(state);

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
    let state = setup_test_state().await;
    let token = get_test_token();

    // Setup Router
    let app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state);

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

#[tokio::test]
async fn test_create_book_success() {
    let state = setup_test_state().await;
    let token = get_test_token();

    let app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state);

    let payload = serde_json::json!({
        "title": "Test Book via Repository",
        "isbn": "9781234567890",
        "author": "Test Author",
        "reading_status": "to_read"
    });

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Verify response contains the book
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["book"]["title"], "Test Book via Repository");
    assert!(json["book"]["id"].as_i64().is_some());
}

#[tokio::test]
async fn test_update_book_success() {
    let state = setup_test_state().await;
    let token = get_test_token();

    // First create a book
    let create_app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state.clone());

    let create_payload = serde_json::json!({
        "title": "Original Title",
        "isbn": "9780987654321"
    });

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&create_payload).unwrap()))
        .unwrap();

    let response = create_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let book_id = json["book"]["id"].as_i64().unwrap();

    // Now update the book
    let update_app = Router::new()
        .route("/books/:id", axum::routing::put(api::books::update_book))
        .with_state(state);

    let update_payload = serde_json::json!({
        "title": "Updated Title",
        "isbn": "9780987654321",
        "reading_status": "reading"
    });

    let req = Request::builder()
        .uri(format!("/books/{}", book_id))
        .method("PUT")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&update_payload).unwrap()))
        .unwrap();

    let response = update_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["book"]["title"], "Updated Title");
    assert_eq!(json["book"]["reading_status"], "reading");
}

#[tokio::test]
async fn test_list_books_with_pagination() {
    let state = setup_test_state().await;
    let token = get_test_token();

    // Create 3 books
    let create_app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state.clone());

    for i in 1..=3 {
        let payload = serde_json::json!({
            "title": format!("Book {}", i),
            "isbn": format!("978000000000{}", i)
        });
        let req = Request::builder()
            .uri("/books")
            .method("POST")
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();
        let response = create_app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // Test list without pagination
    let list_app = Router::new()
        .route("/books", axum::routing::get(api::books::list_books))
        .with_state(state.clone());

    let req = Request::builder()
        .uri("/books")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = list_app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 3);
    assert_eq!(json["books"].as_array().unwrap().len(), 3);

    // Test list with pagination (limit=2, page=0)
    let req = Request::builder()
        .uri("/books?limit=2&page=0")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = list_app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 3); // Total is still 3
    assert_eq!(json["books"].as_array().unwrap().len(), 2); // But only 2 returned
}
