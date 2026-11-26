//! Unit tests for `handle_catalog_delta_request` (ADR-029).
//!
//! The handler is the responder-side bridge between the E2EE relay envelope
//! and the shared `build_book_delta_response` helper. These tests lock the
//! contract: response shape, privacy pipeline (a private book is omitted,
//! except for one bare tombstone in the window where it was made private;
//! every other book goes through `redact_for_peer`),
//! `reset_required` signalling, `has_more` cursor semantics, and tombstone
//! shape. The end-to-end tests at the bottom feed the response into the
//! peer-side `apply_peer_delta_operations` and check the peer's cache.

use rust_lib_app::crypto::envelope::ClearMessage;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{book, copy, operation_log, peer, peer_book};
use rust_lib_app::services::peer_delta_sync::apply_peer_delta_operations;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::{Value, json};

async fn setup() -> AppState {
    // These tests play a delta request answered through the relay: the hub
    // gate (ADR-073), closed by default, is open for them. Closed, a cover
    // is addressed on the local network instead (see hub_silence_test.rs).
    rust_lib_app::services::hub_gate::set_open(true);
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
) -> (String, i32) {
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
        entity_id: Set(book.id.clone()),
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

async fn log_delete(db: &DatabaseConnection, book_id: String) -> i32 {
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
    let _ = log_delete(state.db(), b1.clone()).await;

    let msg = request_message(json!({ "since": after_inserts as i64, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "delete");
    assert_eq!(ops[0]["book_id"], b1);
}

#[tokio::test]
async fn always_private_book_is_omitted_for_peer() {
    let state = setup().await;
    create_book_with_log(state.db(), "Public", false, None).await;
    let (priv_id, _) = create_book_with_log(state.db(), "Private", true, None).await;

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "a book that was never visible sends nothing");
    assert_eq!(ops[0]["book"]["title"], "Public");
    let payload = serde_json::to_string(&resp).unwrap();
    assert!(
        !payload.contains(priv_id.as_str()),
        "the id of a book that was never visible must not leave: {payload}"
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
async fn local_cover_paths_never_leak_as_relative_urls() {
    // Regression: the relay delta handler used to delegate to the LAN
    // variant of the cover rewriter, emitting `/api/books/{id}/cover` for
    // books whose `cover_url` is a local filesystem path. A 5G peer
    // receiving this via hub relay has no base URL to resolve it against,
    // so the cover silently fails to load. Relay mode must rewrite local
    // paths to absolute hub URLs, or strip them to `None` when no hub
    // prefix is configured — never emit a relative path.
    let state = setup().await;
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("With local cover".to_owned()),
        owned: Set(true),
        private: Set(false),
        cover_url: Set(Some("/Users/local/path/cover.jpg".to_owned())),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(state.db())
    .await
    .expect("insert book");

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
    .exec(state.db())
    .await
    .unwrap();

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;

    let payload = serde_json::to_string(&resp).unwrap();
    assert!(
        !payload.contains("/api/books/"),
        "relay delta must never emit relative /api/books/... cover URLs \
         (unresolvable for a peer reaching us via hub relay); got: {payload}"
    );

    // Without HUB_URL set in the test env, `Book::hub_cover_prefix`
    // returns None, so Relay mode strips offending covers rather than
    // failing outright. `cover_url` is `#[serde(skip_serializing_if =
    // "Option::is_none")]` on the DTO, so a stripped value disappears
    // from the JSON entirely.
    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert!(
        ops[0]["book"].get("cover_url").is_none(),
        "expected cover_url stripped with no hub prefix, got {:?}",
        ops[0]["book"].get("cover_url")
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

// End-to-end: the responder's output applied to a peer's cache.

/// A second, independent database standing for the reader who caches the
/// owner's catalog, with the owner registered there as an accepted peer.
async fn peer_side() -> (DatabaseConnection, i32) {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init peer db in memory");
    let now = chrono::Utc::now().to_rfc3339();
    let owner = peer::ActiveModel {
        name: Set("Owner".to_owned()),
        url: Set(format!("http://owner-{}.local", uuid::Uuid::new_v4())),
        key_exchange_done: Set(true),
        connection_status: Set("accepted".to_owned()),
        auto_approve: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert owner peer");
    (db, owner.id)
}

/// Apply `change` to the stored book and log the edit as a local UPDATE.
async fn edit_book(
    db: &DatabaseConnection,
    book_id: &str,
    change: impl FnOnce(&mut book::ActiveModel),
) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut active: book::ActiveModel = book::Entity::find_by_id(book_id.to_owned())
        .one(db)
        .await
        .unwrap()
        .expect("book exists")
        .into();
    let private_before = *active.private.as_ref();
    change(&mut active);
    active.updated_at = Set(now.clone());
    let updated = active.update(db).await.expect("update book");
    // Same marker as the update doors log when a book is made private.
    let payload = (!private_before && updated.private)
        .then(|| rust_lib_app::services::delta_service::peer_withdrawal_payload().to_string());

    operation_log::Entity::insert(operation_log::ActiveModel {
        entity_type: Set("book".to_owned()),
        entity_id: Set(book_id.to_owned()),
        operation: Set("UPDATE".to_owned()),
        payload: Set(payload),
        source: Set("local".to_owned()),
        status: Set("applied".to_owned()),
        created_at: Set(now),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();
}

/// Answer the peer's delta request from `since` and apply the operations to
/// its cache. Returns the cursor the peer keeps for its next request.
async fn sync_peer(
    state: &AppState,
    peer_db: &DatabaseConnection,
    owner_id: i32,
    since: i64,
) -> i64 {
    let msg = request_message(json!({ "since": since, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(state, &msg).await;
    assert_eq!(resp["reset_required"], false);
    let ops = resp["operations"].as_array().expect("operations array");
    apply_peer_delta_operations(peer_db, owner_id, ops)
        .await
        .expect("apply delta on peer side");
    resp["latest_cursor"].as_i64().expect("latest_cursor")
}

async fn cached_row(
    peer_db: &DatabaseConnection,
    owner_id: i32,
    book_id: &str,
) -> Option<peer_book::Model> {
    peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(owner_id))
        .filter(peer_book::Column::RemoteBookId.eq(book_id))
        .one(peer_db)
        .await
        .unwrap()
}

const WISH_ISBN: &str = "9782070360024";

/// Seed an owned, visible book carrying `WISH_ISBN`.
async fn create_book_with_isbn(db: &DatabaseConnection, title: &str) -> String {
    let (id, _) = create_book_with_log(db, title, false, None).await;
    edit_book(db, &id, |b| b.isbn = Set(Some(WISH_ISBN.to_owned()))).await;
    id
}

#[tokio::test]
async fn book_set_private_leaves_peer_cache() {
    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let (kept, _) = create_book_with_log(state.db(), "Kept", false, None).await;
    let (hidden, _) = create_book_with_log(state.db(), "Soon private", false, None).await;

    let cursor = sync_peer(&state, &peer_db, owner_id, 0).await;
    assert!(cached_row(&peer_db, owner_id, &hidden).await.is_some());

    // Through the real update door, which logs the withdrawal itself.
    let mut dto = rust_lib_app::services::book_service::get_book_by_uuid(state.db(), &hidden)
        .await
        .expect("book exists");
    dto.private = Some(true);
    rust_lib_app::services::book_service::update_book(state.db(), &hidden, dto)
        .await
        .expect("update book");
    sync_peer(&state, &peer_db, owner_id, cursor).await;

    assert!(
        cached_row(&peer_db, owner_id, &hidden).await.is_none(),
        "a book made private must leave the peer's cache"
    );
    assert!(cached_row(&peer_db, owner_id, &kept).await.is_some());
}

/// The tombstone is sent once, in the window of the change. Later edits of
/// the private book must stay silent: a repeated tombstone would tell peers
/// when a private book is touched.
#[tokio::test]
async fn private_book_edited_later_sends_nothing() {
    let state = setup().await;
    let (id, _) = create_book_with_log(state.db(), "Soon private", false, None).await;

    edit_book(state.db(), &id, |b| b.private = Set(true)).await;
    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let first = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;
    assert_eq!(
        first["operations"],
        json!([{ "op": "delete", "book_id": id }]),
        "the window of the change carries the id and nothing else"
    );
    let cursor = first["latest_cursor"].as_i64().unwrap();

    edit_book(state.db(), &id, |b| b.title = Set("Renamed".to_owned())).await;
    let msg = request_message(json!({ "since": cursor, "limit": 500 }));
    let second = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;
    assert_eq!(second["operations"], json!([]));
}

/// A later edit in the same window must not hide the withdrawal: the
/// collapse keeps the latest operation, the marker has to outlive it.
#[tokio::test]
async fn withdrawal_survives_a_later_edit_in_the_same_window() {
    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let (id, _) = create_book_with_log(state.db(), "Soon private", false, None).await;
    let cursor = sync_peer(&state, &peer_db, owner_id, 0).await;

    edit_book(state.db(), &id, |b| b.private = Set(true)).await;
    edit_book(state.db(), &id, |b| b.title = Set("Renamed".to_owned())).await;
    sync_peer(&state, &peer_db, owner_id, cursor).await;

    assert!(cached_row(&peer_db, owner_id, &id).await.is_none());
}

/// Made private, then public again before the peer asks: the current state
/// wins and the book travels as a normal upsert.
#[tokio::test]
async fn book_public_again_travels_as_upsert() {
    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let (id, _) = create_book_with_log(state.db(), "Back and forth", false, None).await;
    let cursor = sync_peer(&state, &peer_db, owner_id, 0).await;

    edit_book(state.db(), &id, |b| b.private = Set(true)).await;
    edit_book(state.db(), &id, |b| b.private = Set(false)).await;
    sync_peer(&state, &peer_db, owner_id, cursor).await;

    assert!(cached_row(&peer_db, owner_id, &id).await.is_some());
}

#[tokio::test]
async fn book_no_longer_owned_shows_as_non_owned_in_peer_cache() {
    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let (id, _) = create_book_with_log(state.db(), "Given away", false, None).await;

    let cursor = sync_peer(&state, &peer_db, owner_id, 0).await;
    assert!(cached_row(&peer_db, owner_id, &id).await.unwrap().owned);

    edit_book(state.db(), &id, |b| b.owned = Set(false)).await;
    sync_peer(&state, &peer_db, owner_id, cursor).await;

    let row = cached_row(&peer_db, owner_id, &id)
        .await
        .expect("a non-owned book stays cached");
    assert!(
        !row.owned,
        "the cached row must follow the owner's owned flag"
    );
    assert_eq!(row.wanted, None, "non-owned without a wish is not a wish");
}

#[tokio::test]
async fn book_turned_into_wish_reaches_inverse_wishlist_marker() {
    use rust_lib_app::services::wishlist_service::seekers_for_isbn;

    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let id = create_book_with_isbn(state.db(), "Wanted later").await;

    let cursor = sync_peer(&state, &peer_db, owner_id, 0).await;
    assert!(
        seekers_for_isbn(&peer_db, WISH_ISBN)
            .await
            .unwrap()
            .is_empty()
    );

    edit_book(state.db(), &id, |b| {
        b.owned = Set(false);
        b.reading_status = Set("wanting".to_owned());
    })
    .await;
    sync_peer(&state, &peer_db, owner_id, cursor).await;

    let row = cached_row(&peer_db, owner_id, &id)
        .await
        .expect("wish cached");
    assert!(!row.owned);
    assert_eq!(row.wanted, Some(true));
    let seekers = seekers_for_isbn(&peer_db, WISH_ISBN).await.unwrap();
    assert_eq!(seekers.len(), 1, "the peer sees who wants its copy");
    assert_eq!(seekers[0].peer_id, owner_id);
}

#[tokio::test]
async fn wish_set_private_leaves_peer_cache() {
    use rust_lib_app::services::wishlist_service::seekers_for_isbn;

    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let id = create_book_with_isbn(state.db(), "Secret wish").await;
    edit_book(state.db(), &id, |b| {
        b.owned = Set(false);
        b.reading_status = Set("wanting".to_owned());
    })
    .await;

    let cursor = sync_peer(&state, &peer_db, owner_id, 0).await;
    assert_eq!(
        cached_row(&peer_db, owner_id, &id).await.unwrap().wanted,
        Some(true)
    );

    edit_book(state.db(), &id, |b| b.private = Set(true)).await;
    sync_peer(&state, &peer_db, owner_id, cursor).await;

    assert!(cached_row(&peer_db, owner_id, &id).await.is_none());
    assert!(
        seekers_for_isbn(&peer_db, WISH_ISBN)
            .await
            .unwrap()
            .is_empty(),
        "a private wish must no longer raise the marker"
    );
}

#[tokio::test]
async fn lent_book_stays_owned_in_peer_cache() {
    let state = setup().await;
    let (peer_db, owner_id) = peer_side().await;
    let (id, _) = create_book_with_log(state.db(), "On loan", false, None).await;

    let now = chrono::Utc::now().to_rfc3339();
    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("admin".to_owned()),
        password_hash: Set("hash".to_owned()),
        role: Set("admin".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(state.db())
    .await
    .unwrap();
    let library = rust_lib_app::models::library::ActiveModel {
        name: Set("Library".to_owned()),
        owner_id: Set(user.id),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(state.db())
    .await
    .unwrap();
    copy::ActiveModel {
        book_id: Set(id.clone()),
        library_id: Set(library.id),
        status: Set("loaned".to_owned()),
        is_temporary: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(state.db())
    .await
    .unwrap();

    let msg = request_message(json!({ "since": 0, "limit": 500 }));
    let resp = rust_lib_app::api::e2ee::handle_catalog_delta_request(&state, &msg).await;
    let ops = resp["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["op"], "upsert");
    assert_eq!(ops[0]["book"]["owned"], true);
    assert_eq!(ops[0]["book"]["available_copies"], 0);

    sync_peer(&state, &peer_db, owner_id, 0).await;
    let row = cached_row(&peer_db, owner_id, &id).await.unwrap();
    assert!(row.owned, "a loan does not make the book non-owned");
    assert_eq!(row.available_copies, Some(0));
    assert_eq!(row.wanted, None);
}
