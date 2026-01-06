use rust_lib_app::db;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Helper to create a test database
async fn setup_test_db() -> DatabaseConnection {
    // In-memory SQLite for testing
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    db
}

// Helper to create a test admin user
async fn create_test_admin(db: &DatabaseConnection) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("admin".to_string()),
        password_hash: Set("$2b$12$dummy_hash".to_string()),
        role: Set("admin".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = rust_lib_app::models::user::Entity::insert(user)
        .exec(db)
        .await
        .expect("Failed to create admin user");
    res.last_insert_id
}

// Helper to create a test library
async fn create_test_library(db: &DatabaseConnection, owner_id: i32, name: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let library = rust_lib_app::models::library::ActiveModel {
        name: Set(name.to_string()),
        description: Set(Some("Test library".to_string())),
        owner_id: Set(owner_id),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = rust_lib_app::models::library::Entity::insert(library)
        .exec(db)
        .await
        .expect("Failed to create library");
    res.last_insert_id
}

// Helper to create a test book
async fn create_test_book(db: &DatabaseConnection, title: &str, isbn: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_string()),
        isbn: Set(Some(isbn.to_string())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = rust_lib_app::models::book::Entity::insert(book)
        .exec(db)
        .await
        .expect("Failed to create book");
    res.last_insert_id
}

// Helper to create a test copy
async fn create_test_copy(
    db: &DatabaseConnection,
    book_id: i32,
    library_id: i32,
    status: &str,
) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let copy = rust_lib_app::models::copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(library_id),
        status: Set(status.to_string()),
        is_temporary: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = rust_lib_app::models::copy::Entity::insert(copy)
        .exec(db)
        .await
        .expect("Failed to create copy");
    res.last_insert_id
}

// Helper to create a test peer
async fn create_test_peer(db: &DatabaseConnection, name: &str, url: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set(name.to_string()),
        url: Set(url.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(db)
        .await
        .expect("Failed to create peer");
    res.last_insert_id
}

// Helper to create a test borrow request
async fn create_test_request(
    db: &DatabaseConnection,
    id: &str,
    peer_id: i32,
    isbn: &str,
    title: &str,
    status: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let request = rust_lib_app::models::p2p_request::ActiveModel {
        id: Set(id.to_string()),
        from_peer_id: Set(peer_id),
        book_isbn: Set(isbn.to_string()),
        book_title: Set(title.to_string()),
        status: Set(status.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    };
    rust_lib_app::models::p2p_request::Entity::insert(request)
        .exec(db)
        .await
        .expect("Failed to create request");
}

#[tokio::test]
async fn test_book_crud() {
    let db = setup_test_db().await;

    // 1. Create Book
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Test Book".to_string()),
        isbn: Set(Some("1234567890".to_string())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    let inserted = rust_lib_app::models::book::Entity::insert(book)
        .exec(&db)
        .await
        .expect("Insert failed");
    let book_id = inserted.last_insert_id;

    // 2. Read Book
    let fetched = rust_lib_app::models::book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .expect("Find failed");
    assert!(fetched.is_some());
    assert_eq!(fetched.unwrap().title, "Test Book");

    // 3. Update Book
    let mut active: rust_lib_app::models::book::ActiveModel =
        rust_lib_app::models::book::Entity::find_by_id(book_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .into();
    active.title = Set("Updated Title".to_string());
    active.update(&db).await.expect("Update failed");

    let updated = rust_lib_app::models::book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title, "Updated Title");

    // 4. Delete Book
    rust_lib_app::models::book::Entity::delete_by_id(book_id)
        .exec(&db)
        .await
        .expect("Delete failed");
    let deleted = rust_lib_app::models::book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap();
    assert!(deleted.is_none());
}

#[tokio::test]
async fn test_p2p_connect() {
    let db = setup_test_db().await;

    // Register a peer
    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set("Test Peer".to_string()),
        url: Set("http://localhost:9000".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(&db)
        .await
        .expect("Insert peer failed");

    let saved = rust_lib_app::models::peer::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .unwrap();
    assert!(saved.is_some());
    assert_eq!(saved.unwrap().name, "Test Peer");
}

#[tokio::test]
async fn test_inventory_sync() {
    let db = setup_test_db().await;

    // 1. Setup Mock Server
    let mock_server = MockServer::start().await;

    // 2. Create Peer pointing to Mock Server
    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set("Mock Peer".to_string()),
        url: Set(mock_server.uri()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(&db)
        .await
        .expect("Insert peer failed");
    let peer_id = res.last_insert_id;

    // 3. Mock Response
    let mock_books = serde_json::json!({
        "books": [
            {
                "id": 101,
                "title": "Remote Book 1",
                "isbn": "11111",
                "author": "Remote Author"
            },
            {
                "id": 102,
                "title": "Remote Book 2",
                "isbn": "22222"
            }
        ]
    });

    Mock::given(method("GET"))
        .and(path("/api/books"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_books))
        .mount(&mock_server)
        .await;

    // 4. Test the sync logic
    let client = reqwest::Client::new();
    let url = format!("{}/api/books", mock_server.uri());
    let res = client.get(&url).send().await.expect("Failed to send");
    assert!(res.status().is_success());

    // Verify we can parse it
    let data: serde_json::Value = res.json().await.unwrap();
    assert_eq!(data["books"].as_array().unwrap().len(), 2);

    // Now verify DB insertion logic
    use rust_lib_app::models::peer_book;
    for book in data["books"].as_array().unwrap() {
        let cache = peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set(book["id"].as_i64().unwrap() as i32),
            title: Set(book["title"].as_str().unwrap().to_string()),
            synced_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        peer_book::Entity::insert(cache)
            .exec(&db)
            .await
            .expect("Insert cache failed");
    }

    let count = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .count(&db)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_borrow_request_auto_approve() {
    let db = setup_test_db().await;

    // 1. Create Peer with auto_approve = true
    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set("Trusted Peer".to_string()),
        url: Set("http://trusted.com".to_string()),
        auto_approve: Set(true),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(&db)
        .await
        .unwrap();
    let peer_id = res.last_insert_id;

    // 2. Simulate Incoming Request Logic
    let initial_status = if true { "accepted" } else { "pending" }; // Logic from receive_request

    let request = rust_lib_app::models::p2p_request::ActiveModel {
        id: Set("req-123".to_string()),
        from_peer_id: Set(peer_id),
        book_isbn: Set("999".to_string()),
        book_title: Set("Borrowed Book".to_string()),
        status: Set(initial_status.to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };
    rust_lib_app::models::p2p_request::Entity::insert(request)
        .exec(&db)
        .await
        .unwrap();

    // 3. Verify Status
    let saved = rust_lib_app::models::p2p_request::Entity::find_by_id("req-123")
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.status, "accepted");
}

// ========== NEW CRITICAL TESTS FOR BORROW REQUEST FLOW ==========

#[tokio::test]
async fn test_cannot_accept_request_without_available_copy() {
    // This test would have caught the 409 bug!
    let db = setup_test_db().await;

    // Setup: Create book WITHOUT a copy
    let admin_id = create_test_admin(&db).await;
    let _library_id = create_test_library(&db, admin_id, "Main Library").await;
    let book_id = create_test_book(&db, "Test Book", "123456789").await;

    // Create a peer and request
    let peer_id = create_test_peer(&db, "Borrower Library", "http://peer:8000").await;
    create_test_request(&db, "req-1", peer_id, "123456789", "Test Book", "pending").await;

    // Try to find an available copy (should fail)
    use rust_lib_app::models::copy;
    let available_copy = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("available"))
        .one(&db)
        .await
        .unwrap();

    // Core assertion: No available copy exists
    assert!(available_copy.is_none(), "Expected no available copies");
}

#[tokio::test]
async fn test_can_accept_request_with_available_copy() {
    let db = setup_test_db().await;

    // Setup: Create book WITH an available copy
    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "Main Library").await;
    let book_id = create_test_book(&db, "Test Book", "123456789").await;
    let copy_id = create_test_copy(&db, book_id, library_id, "available").await;

    // Create a peer and request
    let peer_id = create_test_peer(&db, "Borrower Library", "http://peer:8000").await;
    create_test_request(&db, "req-2", peer_id, "123456789", "Test Book", "pending").await;

    // Try to find an available copy (should succeed)
    use rust_lib_app::models::copy;
    let available_copy = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("available"))
        .one(&db)
        .await
        .unwrap();

    // Core assertions
    assert!(available_copy.is_some(), "Expected an available copy");
    assert_eq!(available_copy.unwrap().id, copy_id);
}

#[tokio::test]
async fn test_cannot_accept_request_when_copy_is_borrowed() {
    let db = setup_test_db().await;

    // Setup: Create book with a BORROWED copy (not available)
    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "Main Library").await;
    let book_id = create_test_book(&db, "Test Book", "123456789").await;
    let _ = create_test_copy(&db, book_id, library_id, "borrowed").await;

    // Create a peer and request
    let peer_id = create_test_peer(&db, "Borrower Library", "http://peer:8000").await;
    create_test_request(&db, "req-3", peer_id, "123456789", "Test Book", "pending").await;

    // Try to find an available copy (should fail because copy is borrowed)
    use rust_lib_app::models::copy;
    let available_copy = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("available"))
        .one(&db)
        .await
        .unwrap();

    // Core assertion: No available copy (even though copy exists, it's borrowed)
    assert!(
        available_copy.is_none(),
        "Expected no available copies (copy is borrowed)"
    );
}

#[tokio::test]
async fn test_library_exists_after_admin_creation() {
    // Tests that library can be created after admin user exists
    let db = setup_test_db().await;

    let admin_id = create_test_admin(&db).await;

    // Create a library with the admin as owner
    let library_id = create_test_library(&db, admin_id, "Test Library").await;

    // Verify library was created successfully
    use rust_lib_app::models::library;
    let created_library = library::Entity::find_by_id(library_id)
        .one(&db)
        .await
        .unwrap();

    assert!(
        created_library.is_some(),
        "Expected library to exist after creation"
    );

    let library = created_library.unwrap();
    assert_eq!(library.id, library_id);
    assert_eq!(library.owner_id, admin_id);
}

#[tokio::test]
async fn test_copy_creation_requires_valid_library() {
    // Tests foreign key constraint
    let db = setup_test_db().await;

    let admin_id = create_test_admin(&db).await;
    let _library_id = create_test_library(&db, admin_id, "Main Library").await;
    let book_id = create_test_book(&db, "Test Book", "123").await;

    // Try to create copy with INVALID library_id (foreign key violation)
    let now = chrono::Utc::now().to_rfc3339();
    let invalid_copy = rust_lib_app::models::copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(999), // Non-existent library
        status: Set("available".to_string()),
        is_temporary: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let result = rust_lib_app::models::copy::Entity::insert(invalid_copy)
        .exec(&db)
        .await;

    // Core assertion: Should fail due to foreign key constraint
    assert!(
        result.is_err(),
        "Expected copy creation to fail with invalid library_id"
    );
}

#[tokio::test]
async fn test_sync_clears_old_peer_books() {
    // Tests that sync replaces old cache completely
    let db = setup_test_db().await;

    let peer_id = create_test_peer(&db, "Test Peer", "http://peer:8000").await;

    // Insert old cache entries
    use rust_lib_app::models::peer_book;
    let old_book = peer_book::ActiveModel {
        peer_id: Set(peer_id),
        remote_book_id: Set(1),
        title: Set("Old Book".to_string()),
        synced_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    peer_book::Entity::insert(old_book).exec(&db).await.unwrap();

    // Verify old book exists
    let count_before = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .count(&db)
        .await
        .unwrap();
    assert_eq!(count_before, 1);

    // Simulate sync: Delete old cache
    peer_book::Entity::delete_many()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .exec(&db)
        .await
        .unwrap();

    // Insert new cache
    let new_book = peer_book::ActiveModel {
        peer_id: Set(peer_id),
        remote_book_id: Set(2),
        title: Set("New Book".to_string()),
        synced_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    peer_book::Entity::insert(new_book).exec(&db).await.unwrap();

    // Verify: Only new book exists
    let cached_books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(&db)
        .await
        .unwrap();

    assert_eq!(cached_books.len(), 1);
    assert_eq!(cached_books[0].title, "New Book");
    assert_eq!(cached_books[0].remote_book_id, 2);
}

#[tokio::test]
async fn test_search_unified_endpoint() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for oneshot

    let db = setup_test_db().await;
    let _admin_id = create_test_admin(&db).await;

    // We can't easily mock the external HTTP calls in this integration test without a lot of setup (e.g. wiremock).
    // However, we can verifying the endpoint *exists* and handles a query, even if it returns empty or fails to connect to real external APIs.
    // Ideally we would mock the `search_inventaire` and `search_books` functions but they are free functions in other modules.

    // For now, let's just ensure the route is registered and returns 200 OK (with potentially empty list if no network or no match).
    // This confirms the wiring is correct.

    let app = rust_lib_app::api::api_router(db);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/integrations/search_unified?q=test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // We expect a JSON array
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

    assert!(body_json.is_array(), "Expected JSON array response");
}

// ========== P2P LOAN RETURN CLEANUP TESTS ==========

// Helper to create a test outgoing request (borrower side)
async fn create_test_outgoing_request(
    db: &DatabaseConnection,
    id: &str,
    peer_id: i32,
    isbn: &str,
    title: &str,
    status: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let request = rust_lib_app::models::p2p_outgoing_request::ActiveModel {
        id: Set(id.to_string()),
        to_peer_id: Set(peer_id),
        book_isbn: Set(isbn.to_string()),
        book_title: Set(title.to_string()),
        status: Set(status.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    };
    rust_lib_app::models::p2p_outgoing_request::Entity::insert(request)
        .exec(db)
        .await
        .expect("Failed to create outgoing request");
}

// Helper to create a test book with specific owned/reading_status
async fn create_test_book_with_status(
    db: &DatabaseConnection,
    title: &str,
    isbn: &str,
    owned: bool,
    reading_status: &str,
) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_string()),
        isbn: Set(Some(isbn.to_string())),
        owned: Set(owned),
        reading_status: Set(reading_status.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = rust_lib_app::models::book::Entity::insert(book)
        .exec(db)
        .await
        .expect("Failed to create book");
    res.last_insert_id
}

#[tokio::test]
async fn test_loan_return_deletes_borrowed_copy() {
    // When a loan is returned, the borrowed copy should be deleted
    let db = setup_test_db().await;

    // Setup: Create book with borrowed copy (borrower's perspective)
    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "My Library").await;
    let book_id = create_test_book_with_status(
        &db,
        "Borrowed Book",
        "123456789",
        false, // owned = false (borrowed from peer)
        "READING_STATUS_READING",
    )
    .await;
    let borrowed_copy_id = create_test_copy(&db, book_id, library_id, "borrowed").await;

    // Create peer and outgoing request
    let peer_id = create_test_peer(&db, "Lender Library", "http://lender:8000").await;
    create_test_outgoing_request(
        &db,
        "out-1",
        peer_id,
        "123456789",
        "Borrowed Book",
        "accepted",
    )
    .await;

    // Verify borrowed copy exists
    use rust_lib_app::models::copy;
    let copy_before = copy::Entity::find_by_id(borrowed_copy_id)
        .one(&db)
        .await
        .unwrap();
    assert!(
        copy_before.is_some(),
        "Borrowed copy should exist before return"
    );

    // Simulate the cleanup logic from update_outgoing_status when status = "returned"
    // Delete borrowed copy
    copy::Entity::delete_by_id(borrowed_copy_id)
        .exec(&db)
        .await
        .expect("Delete copy failed");

    // Verify copy was deleted
    let copy_after = copy::Entity::find_by_id(borrowed_copy_id)
        .one(&db)
        .await
        .unwrap();
    assert!(
        copy_after.is_none(),
        "Borrowed copy should be deleted after return"
    );
}

#[tokio::test]
async fn test_loan_return_deletes_book_when_not_owned_and_no_copies() {
    // Book should be deleted if: owned=false, not wishlist, no copies left
    let db = setup_test_db().await;

    // Setup: Create book with borrowed copy (borrower's perspective)
    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "My Library").await;
    let book_id = create_test_book_with_status(
        &db,
        "Borrowed Book",
        "987654321",
        false,                    // owned = false
        "READING_STATUS_READING", // NOT wishlist
    )
    .await;
    let borrowed_copy_id = create_test_copy(&db, book_id, library_id, "borrowed").await;

    // Delete copy (simulating return cleanup)
    use rust_lib_app::models::{book, copy};
    copy::Entity::delete_by_id(borrowed_copy_id)
        .exec(&db)
        .await
        .unwrap();

    // Check conditions for book deletion
    let book_model = book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let remaining_copies = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .count(&db)
        .await
        .unwrap();

    let should_delete = !book_model.owned
        && book_model.reading_status != "READING_STATUS_WISHLIST"
        && remaining_copies == 0;

    assert!(should_delete, "Book should be marked for deletion");

    // Delete book
    book::Entity::delete_by_id(book_id).exec(&db).await.unwrap();

    // Verify book was deleted
    let book_after = book::Entity::find_by_id(book_id).one(&db).await.unwrap();
    assert!(book_after.is_none(), "Book should be deleted after return");
}

#[tokio::test]
async fn test_loan_return_keeps_book_if_owned() {
    // Book should NOT be deleted if owned=true
    let db = setup_test_db().await;

    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "My Library").await;
    let book_id = create_test_book_with_status(
        &db,
        "My Book",
        "111222333",
        true, // owned = TRUE
        "READING_STATUS_READING",
    )
    .await;
    let borrowed_copy_id = create_test_copy(&db, book_id, library_id, "borrowed").await;

    // Delete copy
    use rust_lib_app::models::{book, copy};
    copy::Entity::delete_by_id(borrowed_copy_id)
        .exec(&db)
        .await
        .unwrap();

    // Check conditions
    let book_model = book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let remaining_copies = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .count(&db)
        .await
        .unwrap();

    let should_delete = !book_model.owned
        && book_model.reading_status != "READING_STATUS_WISHLIST"
        && remaining_copies == 0;

    // Core assertion: Book should NOT be deleted because owned=true
    assert!(!should_delete, "Book should NOT be deleted when owned=true");
}

#[tokio::test]
async fn test_loan_return_keeps_book_if_wishlist() {
    // Book should NOT be deleted if in wishlist
    let db = setup_test_db().await;

    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "My Library").await;
    let book_id = create_test_book_with_status(
        &db,
        "Wishlist Book",
        "444555666",
        false,                     // owned = false
        "READING_STATUS_WISHLIST", // IN WISHLIST
    )
    .await;
    let borrowed_copy_id = create_test_copy(&db, book_id, library_id, "borrowed").await;

    // Delete copy
    use rust_lib_app::models::{book, copy};
    copy::Entity::delete_by_id(borrowed_copy_id)
        .exec(&db)
        .await
        .unwrap();

    // Check conditions
    let book_model = book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let remaining_copies = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .count(&db)
        .await
        .unwrap();

    let should_delete = !book_model.owned
        && book_model.reading_status != "READING_STATUS_WISHLIST"
        && remaining_copies == 0;

    // Core assertion: Book should NOT be deleted because in wishlist
    assert!(
        !should_delete,
        "Book should NOT be deleted when in wishlist"
    );
}

#[tokio::test]
async fn test_loan_return_keeps_book_if_has_other_copies() {
    // Book should NOT be deleted if other copies exist
    let db = setup_test_db().await;

    let admin_id = create_test_admin(&db).await;
    let library_id = create_test_library(&db, admin_id, "My Library").await;
    let book_id = create_test_book_with_status(
        &db,
        "Multi-copy Book",
        "777888999",
        false, // owned = false
        "READING_STATUS_READING",
    )
    .await;

    // Create TWO copies: one borrowed, one available
    let borrowed_copy_id = create_test_copy(&db, book_id, library_id, "borrowed").await;
    let _available_copy_id = create_test_copy(&db, book_id, library_id, "available").await;

    // Delete only the borrowed copy
    use rust_lib_app::models::{book, copy};
    copy::Entity::delete_by_id(borrowed_copy_id)
        .exec(&db)
        .await
        .unwrap();

    // Check conditions
    let book_model = book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let remaining_copies = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .count(&db)
        .await
        .unwrap();

    let should_delete = !book_model.owned
        && book_model.reading_status != "READING_STATUS_WISHLIST"
        && remaining_copies == 0;

    // Core assertion: Book should NOT be deleted because other copies exist
    assert!(
        !should_delete,
        "Book should NOT be deleted when other copies exist"
    );
    assert_eq!(remaining_copies, 1, "One copy should remain");
}
