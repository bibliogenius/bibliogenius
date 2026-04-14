//! Integration tests for the delta sync endpoint `GET /api/books?since=X`
//! (ADR-028). Locks the contract between server-side window semantics
//! and the Flutter peer pull loop: response shape, cursor advancement,
//! 410 Gone on stale cursor, and the privacy/regression invariants.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::operation_log;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::Value;
use tower::ServiceExt;

async fn setup() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory")
}

fn build_app(db: DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/books",
            axum::routing::get(rust_lib_app::api::books::list_books),
        )
        .with_state(AppState::new(db))
}

/// Insert a book and a matching operation_log row (source = "local"),
/// returning (book_id, log_id).
async fn create_book_with_log(db: &DatabaseConnection, title: &str, private: bool) -> (i32, i32) {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_owned()),
        owned: Set(true),
        private: Set(private),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert book");

    let log = operation_log::ActiveModel {
        entity_type: Set("book".to_owned()),
        entity_id: Set(book.id),
        operation: Set("INSERT".to_owned()),
        payload: Set(None),
        source: Set("local".to_owned()),
        status: Set("applied".to_owned()),
        created_at: Set(now),
        ..Default::default()
    };
    let log_res = operation_log::Entity::insert(log).exec(db).await.unwrap();
    (book.id, log_res.last_insert_id)
}

async fn log_delete(db: &DatabaseConnection, book_id: i32) -> i32 {
    let row = operation_log::ActiveModel {
        entity_type: Set("book".to_owned()),
        entity_id: Set(book_id),
        operation: Set("DELETE".to_owned()),
        payload: Set(None),
        source: Set("local".to_owned()),
        status: Set("applied".to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    operation_log::Entity::insert(row)
        .exec(db)
        .await
        .unwrap()
        .last_insert_id
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn delta_empty_db_returns_empty_window() {
    let db = setup().await;
    let app = build_app(db);

    let (status, body) = get_json(&app, "/api/books?since=0").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["operations"].as_array().unwrap().len(), 0);
    assert_eq!(body["latest_cursor"], 0);
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn delta_three_inserts_yield_three_upserts() {
    let db = setup().await;
    let (_, _) = create_book_with_log(&db, "Book A", false).await;
    let (_, _) = create_book_with_log(&db, "Book B", false).await;
    let (_, last_log) = create_book_with_log(&db, "Book C", false).await;
    let app = build_app(db);

    let (status, body) = get_json(&app, "/api/books?since=0").await;
    assert_eq!(status, StatusCode::OK);
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 3);
    for op in ops {
        assert_eq!(op["op"], "upsert");
        assert!(op["book"]["title"].is_string());
    }
    assert_eq!(body["latest_cursor"], last_log as i64);
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn delta_after_delete_emits_one_delete() {
    let db = setup().await;
    let (b1, _) = create_book_with_log(&db, "Book A", false).await;
    let (b2, _) = create_book_with_log(&db, "Book B", false).await;
    let (_, after_inserts) = create_book_with_log(&db, "Book C", false).await;
    let _delete_log = log_delete(&db, b2).await;
    let _ = b1;
    let app = build_app(db);

    let uri = format!("/api/books?since={after_inserts}");
    let (status, body) = get_json(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "only the DELETE should be in the window");
    assert_eq!(ops[0]["op"], "delete");
    assert_eq!(ops[0]["book_id"], b2);
}

#[tokio::test]
async fn delta_too_old_cursor_returns_410_gone() {
    let db = setup().await;
    // Build then prune the early log rows so the oldest retained id is high.
    let (_, _) = create_book_with_log(&db, "A", false).await;
    let (_, _) = create_book_with_log(&db, "B", false).await;
    let (_, keep_id) = create_book_with_log(&db, "C", false).await;
    operation_log::Entity::delete_many()
        .filter(operation_log::Column::Id.lt(keep_id))
        .exec(&db)
        .await
        .unwrap();
    let app = build_app(db);

    let (status, body) = get_json(&app, "/api/books?since=0").await;
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["error"], "cursor_too_old");
    assert_eq!(body["oldest_available_cursor"], keep_id as i64);
    assert!(body["hint"].as_str().unwrap().contains("full GET"));
}

#[tokio::test]
async fn delta_omits_private_book_for_peer() {
    let db = setup().await;
    let (_pub_id, _) = create_book_with_log(&db, "Public", false).await;
    let (priv_id, _) = create_book_with_log(&db, "Private", true).await;
    let app = build_app(db);

    let (status, body) = get_json(&app, "/api/books?since=0").await;
    assert_eq!(status, StatusCode::OK);
    let ops = body["operations"].as_array().unwrap();
    // Private book must be entirely omitted (no upsert, no delete tombstone).
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "upsert");
    assert_eq!(ops[0]["book"]["title"], "Public");
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        !serialized.contains(&format!("\"id\":{priv_id}")),
        "private book id must not leak in any form"
    );
}

#[tokio::test]
async fn delta_redacts_cataloguing_notes_for_peer() {
    let db = setup().await;
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Annotated".to_owned()),
        owned: Set(true),
        private: Set(false),
        cataloguing_notes: Set(Some("private notes".to_owned())),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    operation_log::Entity::insert(operation_log::ActiveModel {
        entity_type: Set("book".to_owned()),
        entity_id: Set(book.id),
        operation: Set("INSERT".to_owned()),
        payload: Set(None),
        source: Set("local".to_owned()),
        status: Set("applied".to_owned()),
        created_at: Set(now),
        ..Default::default()
    })
    .exec(&db)
    .await
    .unwrap();
    let app = build_app(db);

    let (status, body) = get_json(&app, "/api/books?since=0").await;
    assert_eq!(status, StatusCode::OK);
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    let book_field = &ops[0]["book"];
    assert!(
        book_field["cataloguing_notes"].is_null(),
        "redact_for_peer must strip cataloguing_notes"
    );
    assert_eq!(book_field["title"], "Annotated");
}

#[tokio::test]
async fn delta_remote_source_rows_are_ignored() {
    let db = setup().await;
    // A book exists, but the only log row is a remote echo. Peer must
    // not see it via delta sync (D1).
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("From peer".to_owned()),
        owned: Set(true),
        private: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    operation_log::Entity::insert(operation_log::ActiveModel {
        entity_type: Set("book".to_owned()),
        entity_id: Set(book.id),
        operation: Set("INSERT".to_owned()),
        payload: Set(None),
        source: Set("device:42".to_owned()),
        status: Set("applied".to_owned()),
        created_at: Set(now),
        ..Default::default()
    })
    .exec(&db)
    .await
    .unwrap();
    let app = build_app(db);

    let (status, body) = get_json(&app, "/api/books?since=0").await;
    assert_eq!(status, StatusCode::OK);
    let ops = body["operations"].as_array().unwrap();
    assert!(ops.is_empty(), "device-sourced rows must not appear");
}

#[tokio::test]
async fn no_since_param_preserves_etag_full_catalog() {
    // Regression check: the existing /api/books contract (full catalog +
    // strong ETag + 304) must remain identical when `?since=` is absent.
    let db = setup().await;
    create_book_with_log(&db, "A", false).await;
    create_book_with_log(&db, "B", false).await;
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
    assert_eq!(first.status(), StatusCode::OK);
    let etag = first
        .headers()
        .get(header::ETAG)
        .expect("ETag header preserved")
        .to_str()
        .unwrap()
        .to_string();

    let second = app
        .oneshot(
            Request::builder()
                .uri("/api/books")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        second.status(),
        StatusCode::NOT_MODIFIED,
        "If-None-Match still short-circuits with 304"
    );
}
