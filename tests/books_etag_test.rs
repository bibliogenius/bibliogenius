//! Integration tests for the If-None-Match / ETag flow on GET /api/books.
//!
//! Catalog sync is on the hot path for peers on 5G (ADR-017 relay + direct
//! HTTP): returning a 304 when the catalog is unchanged shaves the entire
//! response body (~95 KB for a 110-book library) off every refresh. These
//! tests lock the contract so a future refactor cannot silently drop it.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tower::ServiceExt;

async fn setup_db_with_books(count: usize) -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..count {
        let book = rust_lib_app::models::book::ActiveModel {
            title: Set(format!("Book {i}")),
            isbn: Set(Some(format!("978-{i:010}"))),
            owned: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        };
        book.insert(&db).await.expect("insert book");
    }
    db
}

fn build_app(db: DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/books",
            axum::routing::get(rust_lib_app::api::books::list_books),
        )
        .with_state(AppState::new(db))
}

#[tokio::test]
async fn list_books_returns_etag_header() {
    let db = setup_db_with_books(3).await;
    let app = build_app(db);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let etag = resp
        .headers()
        .get(header::ETAG)
        .expect("ETag header required")
        .to_str()
        .unwrap();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "quoted etag");
}

#[tokio::test]
async fn same_catalog_same_etag_across_requests() {
    let db = setup_db_with_books(5).await;
    let app = build_app(db);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag1 = first
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let second = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag2 = second
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert_eq!(etag1, etag2, "deterministic for identical catalog");
}

#[tokio::test]
async fn matching_if_none_match_returns_304_with_empty_body() {
    let db = setup_db_with_books(2).await;
    let app = build_app(db);

    // First request to learn the ETag
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag = first
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Second request with If-None-Match = the etag
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        resp.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        etag,
        "304 must re-emit the same ETag per RFC 7232 §4.1",
    );

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body.is_empty(), "304 body must be empty");
}

#[tokio::test]
async fn non_matching_if_none_match_returns_200_with_body() {
    let db = setup_db_with_books(2).await;
    let app = build_app(db);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .header(header::IF_NONE_MATCH, "\"stale-etag-value\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!body.is_empty(), "200 must carry the catalog body");
}

#[tokio::test]
async fn catalog_mutation_invalidates_etag() {
    let db = setup_db_with_books(2).await;
    let app = build_app(db.clone());

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag_before = first
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Mutate the catalog: add one book.
    let later = chrono::Utc::now().to_rfc3339();
    rust_lib_app::models::book::ActiveModel {
        title: Set("Freshly added".to_string()),
        isbn: Set(Some("978-9999999999".to_string())),
        owned: Set(true),
        created_at: Set(later.clone()),
        updated_at: Set(later),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert new book");

    let second = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .header(header::IF_NONE_MATCH, &etag_before)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        second.status(),
        StatusCode::OK,
        "old etag must not match after a book is added",
    );
    let etag_after = second
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(etag_before, etag_after);
}

#[tokio::test]
async fn wildcard_if_none_match_returns_304() {
    let db = setup_db_with_books(1).await;
    let app = build_app(db);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}
