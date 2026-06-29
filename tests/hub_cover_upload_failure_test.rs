//! Regression tests for the device-local hub-cover-upload retry flag and the
//! `HubDirectoryService::{mark,clear,reset_all}_hub_cover_upload_failure`
//! helpers that drive the owner-side warning badge on the book-details screen
//! when a cover upload to the hub fails.
//!
//! The flag lives in the `book_local` table (a device-local, non-replicated
//! sibling of `books`, ADR-044), not on the `books` row, so it is
//! never carried across account-sync devices.

use rust_lib_app::db;
use rust_lib_app::infrastructure::book_local;
use rust_lib_app::models::book;
use rust_lib_app::services::hub_directory_service::HubDirectoryService;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

async fn insert_book(db: &sea_orm::DatabaseConnection, title: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let active = book::ActiveModel {
        title: Set(title.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    active.insert(db).await.expect("insert book").id
}

/// After `mark_hub_cover_upload_failure`, `book_local` must hold a non-empty
/// ISO 8601 timestamp for the book.
#[tokio::test]
async fn mark_sets_the_failure_timestamp() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let id = insert_book(&db, "Martin Eden").await;

    HubDirectoryService::mark_hub_cover_upload_failure(&db, &id).await;

    let flag = book_local::cover_upload_failed_at(&db, &id)
        .await
        .expect("read flag");
    assert!(
        flag.as_deref().is_some_and(|s| !s.is_empty()),
        "mark must set a non-empty timestamp, got {flag:?}"
    );
}

/// After `clear_hub_cover_upload_failure`, the flag must be gone again so the
/// Flutter UI hides the warning badge.
#[tokio::test]
async fn clear_resets_the_failure_timestamp_to_null() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let id = insert_book(&db, "Les mouches").await;

    HubDirectoryService::mark_hub_cover_upload_failure(&db, &id).await;
    HubDirectoryService::clear_hub_cover_upload_failure(&db, &id).await;

    let flag = book_local::cover_upload_failed_at(&db, &id)
        .await
        .expect("read flag");
    assert!(flag.is_none(), "clear must reset the flag, got {flag:?}");
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

    HubDirectoryService::mark_hub_cover_upload_failure(&db, &id_a).await;
    HubDirectoryService::mark_hub_cover_upload_failure(&db, &id_b).await;

    HubDirectoryService::reset_all_hub_cover_upload_failures(&db).await;

    for id in [id_a, id_b, id_clean] {
        let flag = book_local::cover_upload_failed_at(&db, &id)
            .await
            .expect("read flag");
        assert!(flag.is_none(), "book {id} must be cleared, got {flag:?}");
    }
}

/// Marking an unknown book must not panic and must not touch the `books` table
/// (the helpers are side-effect only and swallow DB errors so the surrounding
/// sync loop stays alive). It may create an inert `book_local` row, which is
/// harmless and cleared by `clear`/`reset_all`; `mark` is only ever called with
/// real book ids in production.
#[tokio::test]
async fn mark_unknown_book_is_a_noop() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");

    HubDirectoryService::mark_hub_cover_upload_failure(&db, "9999").await;

    let count = book::Entity::find().all(&db).await.expect("find all").len();
    assert_eq!(
        count, 0,
        "no books row should be created for an unknown book id"
    );
}
