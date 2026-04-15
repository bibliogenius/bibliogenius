//! Unit tests for `handle_catalog_delta_request` (ADR-029).
//!
//! The handler is the responder-side bridge between the E2EE relay envelope
//! and the shared `build_book_delta_response` helper. These tests lock the
//! contract: response shape, privacy pipeline (private-book omission and
//! `redact_for_peer`), `reset_required` signalling, `has_more` cursor
//! semantics, and tombstone shape.

use rust_lib_app::crypto::envelope::ClearMessage;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::operation_log;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::{Value, json};

async fn setup() -> AppState {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    AppState::new(db)
}

fn request_message(payload: Value) -> ClearMessage {
    ClearMessage {
        message_type: "catalog_delta_request".to_string(),
        payload,
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: Some(uuid::Uuid::new_v4().to_string()),
        reply_to_mailbox: None,
        reply_to_write_token: None,
    }
}

async fn create_book_with_log(
    db: &DatabaseConnection,
    title: &str,
    private: bool,
    cataloguing_notes: Option<&str>,
) -> (i32, i32) {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_owned()),
        owned: Set(true),
        private: Set(private),
        cataloguing_notes: Set(cataloguing_notes.map(|s| s.to_owned())),
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

#[tokio::test]
async fn empty_log_returns_empty_delta() {
    let state = setup().await;
    let msg = request_message(json!({ "since": null, "limit": 500 }));

    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    assert_eq!(resp["operations"].as_array().unwrap().len(), 0);
    assert_eq!(resp["reset_required"], false);
    assert_eq!(resp["has_more"], false);
    assert_eq!(resp["latest_cursor"], 0);
}

#[tokio::test]
async fn missing_since_is_treated_as_first_sync() {
    let state = setup().await;
    let (_id, log_id) = create_book_with_log(state.db(), "Book A", false, None).await;
    // Payload without "since" at all — handler must interpret as None / first sync.
    let msg = request_message(json!({ "limit": 500 }));

    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "upsert");
    assert_eq!(resp["latest_cursor"], log_id as i64);
}

#[tokio::test]
async fn three_inserts_emit_three_upserts() {
    let state = setup().await;
    create_book_with_log(state.db(), "A", false, None).await;
    create_book_with_log(state.db(), "B", false, None).await;
    let (_, last) = create_book_with_log(state.db(), "C", false, None).await;
    let msg = request_message(json!({ "since": 0, "limit": 500 }));

    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 3);
    for op in ops {
        assert_eq!(op["op"], "upsert");
    }
    assert_eq!(resp["latest_cursor"], last as i64);
    assert_eq!(resp["reset_required"], false);
}

#[tokio::test]
async fn delete_emits_tombstone() {
    let state = setup().await;
    let (b1, _) = create_book_with_log(state.db(), "A", false, None).await;
    let (_, after_inserts) = create_book_with_log(state.db(), "B", false, None).await;
    let _ = log_delete(state.db(), b1).await;

    let msg = request_message(json!({ "since": after_inserts as i64, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "delete");
    assert_eq!(ops[0]["book_id"], b1);
}

#[tokio::test]
async fn private_book_is_omitted_for_peer() {
    let state = setup().await;
    create_book_with_log(state.db(), "Public", false, None).await;
    let (priv_id, _) = create_book_with_log(state.db(), "Private", true, None).await;

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "private book must be dropped, not tombstoned");
    assert_eq!(ops[0]["book"]["title"], "Public");
    let payload = serde_json::to_string(&resp).unwrap();
    assert!(
        !payload.contains(&format!("\"id\":{priv_id}")),
        "private book id must not appear anywhere in the response"
    );
}

#[tokio::test]
async fn cataloguing_notes_are_stripped_for_peer() {
    let state = setup().await;
    create_book_with_log(state.db(), "Annotated", false, Some("private notes")).await;

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    let book = &ops[0]["book"];
    assert_eq!(book["title"], "Annotated");
    assert!(
        book["cataloguing_notes"].is_null(),
        "redact_for_peer must run on relay path",
    );
}

#[tokio::test]
async fn stale_cursor_returns_reset_required() {
    let state = setup().await;
    let (_, _) = create_book_with_log(state.db(), "A", false, None).await;
    let (_, _) = create_book_with_log(state.db(), "B", false, None).await;
    let (_, keep) = create_book_with_log(state.db(), "C", false, None).await;
    // Simulate retention prune: drop the early rows so cursor=0 is stale.
    operation_log::Entity::delete_many()
        .filter(operation_log::Column::Id.lt(keep))
        .exec(state.db())
        .await
        .unwrap();

    let stale_cursor: i64 = 0;
    let msg = request_message(json!({ "since": stale_cursor, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    assert_eq!(resp["reset_required"], true);
    assert_eq!(resp["operations"].as_array().unwrap().len(), 0);
    assert_eq!(
        resp["latest_cursor"], stale_cursor,
        "reset_required echoes the caller's cursor unchanged so the client can re-request after fallback",
    );
    assert_eq!(resp["has_more"], false);
    // The responder MUST surface its current log max so a requester that
    // runs the legacy full-catalog fallback can adopt the value afterwards
    // and break out of the reset loop (see ADR-029 IN12 / peer_delta_sync
    // set_peer_last_delta_cursor).
    let current_cursor = resp["current_cursor"]
        .as_i64()
        .expect("current_cursor must be populated on reset_required");
    assert_eq!(
        current_cursor, keep as i64,
        "current_cursor must be the retained row id (global operation_log max)",
    );
}

#[tokio::test]
async fn reset_required_current_cursor_tracks_latest_op() {
    // Add more local ops after the stale point and confirm current_cursor
    // advances accordingly — locking the invariant that current_cursor is
    // always "oldest_or_latest_cursor()" of the responder at response time.
    let state = setup().await;
    let (_, _) = create_book_with_log(state.db(), "A", false, None).await;
    let (_, keep) = create_book_with_log(state.db(), "B", false, None).await;
    operation_log::Entity::delete_many()
        .filter(operation_log::Column::Id.lt(keep))
        .exec(state.db())
        .await
        .unwrap();
    let (_, latest) = create_book_with_log(state.db(), "C", false, None).await;

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    assert_eq!(resp["reset_required"], true);
    assert_eq!(
        resp["current_cursor"].as_i64().unwrap(),
        latest as i64,
        "current_cursor must be the latest id at response time, not the retained oldest",
    );
}

#[tokio::test]
async fn has_more_signals_when_window_capped() {
    let state = setup().await;
    for i in 0..5 {
        create_book_with_log(state.db(), &format!("Book {i}"), false, None).await;
    }

    let msg = request_message(json!({ "since": 0, "limit": 2 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    assert_eq!(resp["has_more"], true);
    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(resp["reset_required"], false);
    let latest = resp["latest_cursor"].as_i64().unwrap();
    assert!(
        latest > 0 && latest < 5,
        "latest_cursor must be the last log id in the window, not the global max",
    );
}

#[tokio::test]
async fn remote_source_rows_are_ignored() {
    // Rows written by `source=device:X` must not surface via delta sync (D1).
    let state = setup().await;
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("From peer".to_owned()),
        owned: Set(true),
        private: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(state.db())
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
    .exec(state.db())
    .await
    .unwrap();

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    assert!(resp["operations"].as_array().unwrap().is_empty());
}
