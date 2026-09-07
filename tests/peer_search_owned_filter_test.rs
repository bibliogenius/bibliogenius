//! A peer searching my library must see the same books my catalogue shows.
//!
//! Books I do not own (wishes, copies borrowed from someone else, books read at
//! someone else's place) are never shared: the full-catalogue path filters them
//! through `owned_only=true`, the delta path omits them explicitly, and the
//! directory push filters them through `public_catalog_condition`. `POST
//! /peers/search` is the fourth outbound lane, the one a peer calls directly,
//! and it used to answer with everything that was not private.
//!
//! The visible symptom: a peer searching my shelves found a book I had merely
//! read at their place, and could ask to borrow a book I never had.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rust_lib_app::api::api_router;
use rust_lib_app::db;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tower::ServiceExt;

async fn setup_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory")
}

async fn insert_book(db: &DatabaseConnection, title: &str, owned: bool, reading_status: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_string()),
        owned: Set(owned),
        private: Set(false),
        reading_status: Set(reading_status.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert book");
}

async fn search_titles(db: DatabaseConnection, query: &str) -> Vec<String> {
    let request = Request::builder()
        .method("POST")
        .uri("/peers/search")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"query":"{query}"}}"#)))
        .expect("request");

    let response = api_router(db).oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let books: Vec<serde_json::Value> = serde_json::from_slice(&body).expect("json array");
    books
        .iter()
        .filter_map(|b| b.get("title").and_then(|t| t.as_str()).map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_peer_search_answers_only_with_books_i_own() {
    let db = setup_db().await;
    insert_book(&db, "Martin Eden", true, "read").await;
    insert_book(&db, "Martin Eden lu ailleurs", false, "read").await;

    let titles = search_titles(db, "Martin Eden").await;

    assert_eq!(
        titles,
        vec!["Martin Eden".to_string()],
        "a book read but not owned must not surface in a peer's search"
    );
}

#[tokio::test]
async fn a_peer_search_hides_a_wish_and_a_borrowed_copy() {
    let db = setup_db().await;
    insert_book(&db, "Souhait", false, "wanting").await;
    insert_book(&db, "Souhait exauce", true, "to_read").await;

    let titles = search_titles(db, "Souhait").await;

    assert_eq!(
        titles,
        vec!["Souhait exauce".to_string()],
        "a wish is not a book on my shelves"
    );
}
