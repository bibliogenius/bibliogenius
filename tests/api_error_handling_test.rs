use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use rust_lib_app::api;
use rust_lib_app::auth;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tower::util::ServiceExt; // for `oneshot`

// Helper to create a test app state
async fn setup_test_state() -> AppState {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    AppState::new(db)
}

// Helper to create a test admin user
async fn create_test_admin(db: &DatabaseConnection) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let admin = rust_lib_app::models::user::ActiveModel {
        username: Set("test_admin".to_string()),
        password_hash: Set("hash".to_string()),
        role: Set("admin".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = admin.insert(db).await.expect("Failed to create admin");
    res.id
}

// Helper to create a test library
async fn create_test_library(db: &DatabaseConnection, owner_id: i32) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let library = rust_lib_app::models::library::ActiveModel {
        name: Set("Test Library".to_string()),
        owner_id: Set(owner_id),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = library.insert(db).await.expect("Failed to create library");
    res.id
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

#[tokio::test]
async fn test_author_crud_via_repository() {
    let state = setup_test_state().await;

    // Create author
    let create_app = Router::new()
        .route("/authors", axum::routing::post(api::author::create_author))
        .with_state(state.clone());

    let payload = serde_json::json!({ "name": "Test Author" });
    let req = Request::builder()
        .uri("/authors")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = create_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let author_id = json["id"].as_i64().unwrap();
    assert_eq!(json["name"], "Test Author");

    // Get author
    let get_app = Router::new()
        .route("/authors/:id", axum::routing::get(api::author::get_author))
        .with_state(state.clone());

    let req = Request::builder()
        .uri(format!("/authors/{}", author_id))
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = get_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // List authors
    let list_app = Router::new()
        .route("/authors", axum::routing::get(api::author::list_authors))
        .with_state(state.clone());

    let req = Request::builder()
        .uri("/authors")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = list_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.as_array().unwrap().len() >= 1);

    // Delete author
    let delete_app = Router::new()
        .route(
            "/authors/:id",
            axum::routing::delete(api::author::delete_author),
        )
        .with_state(state);

    let req = Request::builder()
        .uri(format!("/authors/{}", author_id))
        .method("DELETE")
        .body(Body::empty())
        .unwrap();

    let response = delete_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_copy_crud_via_repository() {
    let state = setup_test_state().await;
    let token = get_test_token();

    // Create admin and library first (required for foreign key)
    let admin_id = create_test_admin(state.db()).await;
    let library_id = create_test_library(state.db(), admin_id).await;

    // First create a book (required for copy)
    let create_book_app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state.clone());

    let book_payload = serde_json::json!({
        "title": "Test Book for Copy",
        "isbn": "9781234567891"
    });

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&book_payload).unwrap()))
        .unwrap();

    let response = create_book_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let book_id = json["book"]["id"].as_i64().unwrap() as i32;

    // Create a copy
    let create_copy_app = Router::new()
        .route("/copies", axum::routing::post(api::copy::create_copy))
        .with_state(state.clone());

    let copy_payload = serde_json::json!({
        "book_id": book_id,
        "library_id": library_id,
        "status": "available",
        "is_temporary": false,
        "notes": "Test copy"
    });

    let req = Request::builder()
        .uri("/copies")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&copy_payload).unwrap()))
        .unwrap();

    let response = create_copy_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let copy_id = json["copy"]["id"].as_i64().unwrap();
    assert_eq!(json["copy"]["status"], "available");
    assert_eq!(json["copy"]["notes"], "Test copy");

    // Get single copy
    let get_copy_app = Router::new()
        .route("/copies/:id", axum::routing::get(api::copy::get_copy))
        .with_state(state.clone());

    let req = Request::builder()
        .uri(format!("/copies/{}", copy_id))
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = get_copy_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // List copies
    let list_copies_app = Router::new()
        .route("/copies", axum::routing::get(api::copy::list_copies))
        .with_state(state.clone());

    let req = Request::builder()
        .uri("/copies")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = list_copies_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["total"].as_i64().unwrap() >= 1);

    // Get book copies
    let get_book_copies_app = Router::new()
        .route(
            "/books/:id/copies",
            axum::routing::get(api::copy::get_book_copies),
        )
        .with_state(state.clone());

    let req = Request::builder()
        .uri(format!("/books/{}/copies", book_id))
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = get_book_copies_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Should have exactly 1 copy for this book (or at least 1)
    assert!(json["total"].as_i64().unwrap() >= 1);

    // Update copy
    let update_copy_app = Router::new()
        .route("/copies/:id", axum::routing::put(api::copy::update_copy))
        .with_state(state.clone());

    let update_payload = serde_json::json!({
        "status": "borrowed",
        "notes": "Updated notes"
    });

    let req = Request::builder()
        .uri(format!("/copies/{}", copy_id))
        .method("PUT")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&update_payload).unwrap()))
        .unwrap();

    let response = update_copy_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["copy"]["status"], "borrowed");
    assert_eq!(json["copy"]["notes"], "Updated notes");

    // Delete copy
    let delete_copy_app = Router::new()
        .route("/copies/:id", axum::routing::delete(api::copy::delete_copy))
        .with_state(state.clone());

    let req = Request::builder()
        .uri(format!("/copies/{}", copy_id))
        .method("DELETE")
        .body(Body::empty())
        .unwrap();

    let response = delete_copy_app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify idempotent delete
    let req = Request::builder()
        .uri(format!("/copies/{}", copy_id))
        .method("DELETE")
        .body(Body::empty())
        .unwrap();

    let response = delete_copy_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_get_copy_not_found() {
    let state = setup_test_state().await;

    let app = Router::new()
        .route("/copies/:id", axum::routing::get(api::copy::get_copy))
        .with_state(state);

    let req = Request::builder()
        .uri("/copies/99999")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_update_copy_not_found() {
    let state = setup_test_state().await;

    let app = Router::new()
        .route("/copies/:id", axum::routing::put(api::copy::update_copy))
        .with_state(state);

    let payload = serde_json::json!({
        "status": "borrowed"
    });

    let req = Request::builder()
        .uri("/copies/99999")
        .method("PUT")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_borrowed_copies_via_repository() {
    let state = setup_test_state().await;
    let token = get_test_token();

    // Create admin and library first
    let admin_id = create_test_admin(state.db()).await;
    let library_id = create_test_library(state.db(), admin_id).await;

    // Create a book
    let create_book_app = Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state.clone());

    let book_payload = serde_json::json!({
        "title": "Borrowed Book Test",
        "isbn": "9781234567892"
    });

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&book_payload).unwrap()))
        .unwrap();

    let response = create_book_app.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let book_id = json["book"]["id"].as_i64().unwrap() as i32;

    // Create a borrowed copy (is_temporary=true)
    let create_copy_app = Router::new()
        .route("/copies", axum::routing::post(api::copy::create_copy))
        .with_state(state.clone());

    let copy_payload = serde_json::json!({
        "book_id": book_id,
        "library_id": library_id,
        "status": "borrowed",
        "is_temporary": true,
        "notes": "Borrowed from: Test User"
    });

    let req = Request::builder()
        .uri("/copies")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&copy_payload).unwrap()))
        .unwrap();

    let response = create_copy_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Get borrowed copies
    let borrowed_app = Router::new()
        .route(
            "/copies/borrowed",
            axum::routing::get(api::copy::get_borrowed_copies),
        )
        .with_state(state);

    let req = Request::builder()
        .uri("/copies/borrowed")
        .method("GET")
        .body(Body::empty())
        .unwrap();

    let response = borrowed_app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Should return "loans" key for Flutter compatibility
    assert!(json["loans"].is_array());
    assert!(json["total"].as_i64().unwrap() >= 1);

    // Verify loan structure
    let loans = json["loans"].as_array().unwrap();
    let loan = loans.iter().find(|l| l["title"] == "Borrowed Book Test");
    assert!(loan.is_some());
    let loan = loan.unwrap();
    assert_eq!(loan["notes"], "Borrowed from: Test User");
    assert_eq!(loan["from_contact"], "Borrowed from: Test User");
}
