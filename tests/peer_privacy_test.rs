//! Privacy tests for peer-facing catalog endpoints (E1 / Option B).
//!
//! Ticket E background: /api/books, /api/books/:id were returning the full
//! Book DTO — including personal annotations (user_rating, reading_status,
//! cataloguing_notes, price, shelf_position, source_data, finished/started
//! reading timestamps) — to ANY unauthenticated caller on the LAN. This
//! leaks personal data to strangers on shared WiFi.
//!
//! Decision: keep the endpoints publicly reachable (browse-before-pairing
//! UX, mDNS discovery preserved per ADR-026), but:
//! - Redact personal fields from the response when no valid JWT is present.
//! - Filter out `private=true` books from unauthenticated responses.
//!
//! Authenticated callers (Flutter web / MCP / CLI with a valid owner JWT)
//! continue to see the full DTO — it is their own library.
//!
//! Scope (E1 is intentionally narrow):
//! - /api/books, /api/books/:id only.
//! - /api/books/:id/cover is a binary endpoint (no DTO) — unchanged.
//! - Peer-action endpoints (search, request, requests/incoming, ...) are
//!   NOT covered here — see ticket E2 (HMAC auth, deferred).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rust_lib_app::auth::create_jwt;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use serde_json::Value;
use tower::ServiceExt;

async fn setup_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory")
}

async fn insert_rich_book(db: &DatabaseConnection, title: &str, private: bool) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_string()),
        isbn: Set(Some("9780000000001".to_string())),
        summary: Set(Some("Public summary".to_string())),
        publisher: Set(Some("ACME".to_string())),
        publication_year: Set(Some(2020)),
        owned: Set(true),
        private: Set(private),
        reading_status: Set("reading".to_string()),
        user_rating: Set(Some(9)),
        cataloguing_notes: Set(Some("Personal thoughts about this book".to_string())),
        shelf_position: Set(Some(42)),
        price: Set(Some(19.90)),
        source_data: Set(Some(
            r#"{"source":"amazon","url":"https://..."}"#.to_string(),
        )),
        finished_reading_at: Set(Some("2026-03-01".to_string())),
        started_reading_at: Set(Some("2026-02-15".to_string())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book.insert(db).await.expect("insert book").id
}

fn build_app(db: DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/books",
            axum::routing::get(rust_lib_app::api::books::list_books),
        )
        .route(
            "/api/books/:id",
            axum::routing::get(rust_lib_app::api::books::get_book),
        )
        .with_state(AppState::new(db))
}

async fn get_json(app: &axum::Router, uri: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(uri).method("GET");
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn personal_fields() -> &'static [&'static str] {
    &[
        "user_rating",
        "cataloguing_notes",
        "reading_status",
        "shelf_position",
        "price",
        "source_data",
        "finished_reading_at",
        "started_reading_at",
    ]
}

// ── /api/books (list) ─────────────────────────────────────────────────

#[tokio::test]
async fn list_books_unauth_strips_personal_fields() {
    let db = setup_db().await;
    insert_rich_book(&db, "Book A", false).await;
    let app = build_app(db);

    let (status, json) = get_json(&app, "/api/books", None).await;
    assert_eq!(status, StatusCode::OK);
    let books = json.get("books").and_then(|v| v.as_array()).unwrap();
    assert_eq!(books.len(), 1, "public book must still appear");
    let first = &books[0];

    // Catalog fields that must remain for browsability.
    assert_eq!(first.get("title").and_then(|v| v.as_str()), Some("Book A"));
    assert!(first.get("isbn").is_some(), "ISBN must remain public");
    assert!(first.get("summary").is_some(), "summary must remain public");

    // Personal fields must NOT appear in the JSON (either absent or null).
    for field in personal_fields() {
        let v = first.get(field);
        assert!(
            v.is_none() || v.map(|x| x.is_null()).unwrap_or(true),
            "unauth response must not leak personal field `{field}`: got {v:?}"
        );
    }
}

#[tokio::test]
async fn list_books_auth_keeps_personal_fields() {
    let db = setup_db().await;
    insert_rich_book(&db, "Book A", false).await;
    let app = build_app(db);

    let token = create_jwt("owner", "admin").expect("jwt");
    let (status, json) = get_json(&app, "/api/books", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let books = json.get("books").and_then(|v| v.as_array()).unwrap();
    let first = &books[0];

    assert_eq!(first.get("user_rating").and_then(|v| v.as_i64()), Some(9));
    assert_eq!(
        first.get("reading_status").and_then(|v| v.as_str()),
        Some("reading")
    );
    assert_eq!(
        first.get("cataloguing_notes").and_then(|v| v.as_str()),
        Some("Personal thoughts about this book")
    );
    assert_eq!(
        first.get("shelf_position").and_then(|v| v.as_i64()),
        Some(42)
    );
    assert_eq!(first.get("price").and_then(|v| v.as_f64()), Some(19.90));
}

#[tokio::test]
async fn list_books_unauth_filters_private_books() {
    let db = setup_db().await;
    insert_rich_book(&db, "Public Book", false).await;
    insert_rich_book(&db, "Private Book", true).await;
    let app = build_app(db);

    let (status, json) = get_json(&app, "/api/books", None).await;
    assert_eq!(status, StatusCode::OK);
    let books = json.get("books").and_then(|v| v.as_array()).unwrap();
    assert_eq!(books.len(), 1, "only the public book must be listed");
    assert_eq!(
        books[0].get("title").and_then(|v| v.as_str()),
        Some("Public Book")
    );
    // Total must reflect the filtered count, not the raw DB count.
    assert_eq!(json.get("total").and_then(|v| v.as_u64()), Some(1));
}

#[tokio::test]
async fn list_books_auth_returns_private_books() {
    let db = setup_db().await;
    insert_rich_book(&db, "Public Book", false).await;
    insert_rich_book(&db, "Private Book", true).await;
    let app = build_app(db);

    let token = create_jwt("owner", "admin").expect("jwt");
    let (_, json) = get_json(&app, "/api/books", Some(&token)).await;
    let books = json.get("books").and_then(|v| v.as_array()).unwrap();
    assert_eq!(books.len(), 2, "owner must see both public + private books");
}

// ── /api/books/:id (detail) ───────────────────────────────────────────

#[tokio::test]
async fn get_book_unauth_strips_personal_fields() {
    let db = setup_db().await;
    let id = insert_rich_book(&db, "Book A", false).await;
    let app = build_app(db);

    let (status, json) = get_json(&app, &format!("/api/books/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(json.get("title").and_then(|v| v.as_str()), Some("Book A"));
    for field in personal_fields() {
        let v = json.get(field);
        assert!(
            v.is_none() || v.map(|x| x.is_null()).unwrap_or(true),
            "unauth detail must not leak `{field}`: got {v:?}"
        );
    }
}

#[tokio::test]
async fn get_book_auth_keeps_personal_fields() {
    let db = setup_db().await;
    let id = insert_rich_book(&db, "Book A", false).await;
    let app = build_app(db);

    let token = create_jwt("owner", "admin").expect("jwt");
    let (status, json) = get_json(&app, &format!("/api/books/{id}"), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.get("user_rating").and_then(|v| v.as_i64()), Some(9));
    assert_eq!(
        json.get("cataloguing_notes").and_then(|v| v.as_str()),
        Some("Personal thoughts about this book")
    );
}

#[tokio::test]
async fn get_book_unauth_private_returns_404() {
    // Returning 404 rather than 403 avoids confirming the existence of a
    // private book to anonymous callers.
    let db = setup_db().await;
    let id = insert_rich_book(&db, "Private Book", true).await;
    let app = build_app(db);

    let (status, _) = get_json(&app, &format!("/api/books/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_book_auth_private_returns_full() {
    let db = setup_db().await;
    let id = insert_rich_book(&db, "Private Book", true).await;
    let app = build_app(db);

    let token = create_jwt("owner", "admin").expect("jwt");
    let (status, json) = get_json(&app, &format!("/api/books/{id}"), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json.get("user_rating").and_then(|v| v.as_i64()), Some(9));
}
