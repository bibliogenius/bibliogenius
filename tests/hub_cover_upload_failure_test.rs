//! Regression tests for migration 072 (hub_cover_upload_failed_at) and the
//! `HubDirectoryService::{mark,clear,reset_all}_hub_cover_upload_failure`
//! helpers that drive the owner-side warning badge on the book-details
//! screen when a cover upload to the hub fails.

use rust_lib_app::db;
use rust_lib_app::models::book;
use rust_lib_app::services::hub_directory_service::HubDirectoryService;
use sea_orm::{EntityTrait, Set};

async fn insert_book(db: &sea_orm::DatabaseConnection, title: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let active = book::ActiveModel {
        title: Set(title.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book::Entity::insert(active)
        .exec(db)
        .await
        .expect("insert book")
        .last_insert_id
}

/// After `mark_hub_cover_upload_failure`, `books.hub_cover_upload_failed_at`
/// must be populated with a non-empty ISO 8601 timestamp.
#[tokio::test]
async fn mark_sets_the_failure_timestamp() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let id = insert_book(&db, "Martin Eden").await;

    HubDirectoryService::mark_hub_cover_upload_failure(&db, id).await;

    let row = book::Entity::find_by_id(id)
        .one(&db)
        .await
        .expect("find")
        .expect("row exists");
    assert!(
        row.hub_cover_upload_failed_at
            .as_deref()
            .is_some_and(|s| !s.is_empty()),
        "mark must set a non-empty timestamp, got {:?}",
        row.hub_cover_upload_failed_at
    );
}

/// After `clear_hub_cover_upload_failure`, the flag must be NULL again so
/// the Flutter UI hides the warning badge.
#[tokio::test]
async fn clear_resets_the_failure_timestamp_to_null() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let id = insert_book(&db, "Les mouches").await;

    HubDirectoryService::mark_hub_cover_upload_failure(&db, id).await;
    HubDirectoryService::clear_hub_cover_upload_failure(&db, id).await;

    let row = book::Entity::find_by_id(id)
        .one(&db)
        .await
        .expect("find")
        .expect("row exists");
    assert!(
        row.hub_cover_upload_failed_at.is_none(),
        "clear must reset to NULL, got {:?}",
        row.hub_cover_upload_failed_at
    );
}

/// `reset_all_hub_cover_upload_failures` must clear every flagged book in a
/// single call. Used by the hub purge path so stale warnings do not survive
/// an unregister / re-register cycle.
#[tokio::test]
async fn reset_all_clears_every_book() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let id_a = insert_book(&db, "A").await;
    let id_b = insert_book(&db, "B").await;
    let id_clean = insert_book(&db, "C").await;

    HubDirectoryService::mark_hub_cover_upload_failure(&db, id_a).await;
    HubDirectoryService::mark_hub_cover_upload_failure(&db, id_b).await;

    HubDirectoryService::reset_all_hub_cover_upload_failures(&db).await;

    for id in [id_a, id_b, id_clean] {
        let row = book::Entity::find_by_id(id)
            .one(&db)
            .await
            .expect("find")
            .expect("row exists");
        assert!(
            row.hub_cover_upload_failed_at.is_none(),
            "book {id} must be cleared, got {:?}",
            row.hub_cover_upload_failed_at
        );
    }
}

/// Marking an unknown book must not create a row and must not panic: the
/// helpers are side-effect only and swallow DB errors so the surrounding
/// sync loop stays alive.
#[tokio::test]
async fn mark_unknown_book_is_a_noop() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");

    HubDirectoryService::mark_hub_cover_upload_failure(&db, 9999).await;

    let count = book::Entity::find().all(&db).await.expect("find all").len();
    assert_eq!(count, 0, "no row should be created for an unknown book id");
}
