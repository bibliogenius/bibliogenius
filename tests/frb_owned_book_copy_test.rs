//! An owned book created through the FFI path gets its copy, even on a fresh
//! install.
//!
//! The Flutter app forces `owned: false` on FFI creation and then adds the
//! copy over HTTP, because the native path once failed with NotFound
//! ("missing Library Context"). `book_service::create_book` now resolves the
//! library through `resolve_library_id`, which bootstraps the row: this test
//! drives the same conversion the bridge uses (`FrbBook` -> `models::Book`)
//! on an empty database to check the native path stands on its own.

use rust_lib_app::api::frb::FrbBook;
use rust_lib_app::db;
use rust_lib_app::models::copy;
use rust_lib_app::services::book_service;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

fn frb_book(owned: bool) -> FrbBook {
    FrbBook {
        id: None,
        title: "Le Petit Prince".to_string(),
        author: Some("Antoine de Saint-Exupéry".to_string()),
        isbn: Some("9782070612758".to_string()),
        summary: None,
        publisher: None,
        publication_year: None,
        cover_url: None,
        large_cover_url: None,
        reading_status: None,
        shelf_position: None,
        user_rating: None,
        subjects: None,
        created_at: None,
        updated_at: None,
        finished_reading_at: None,
        started_reading_at: None,
        owned,
        price: None,
        digital_formats: None,
        private: false,
        page_count: None,
        added_at: None,
        hub_cover_upload_failed_at: None,
        is_borrowed: None,
        is_lent: None,
        wanted: None,
        wished_by: None,
    }
}

async fn copies_of(db: &DatabaseConnection, book_id: &str) -> Vec<copy::Model> {
    copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .all(db)
        .await
        .expect("query copies")
}

#[tokio::test]
async fn owned_frb_book_gets_an_available_copy_on_a_fresh_db() {
    let db = setup_test_db().await;

    let created = book_service::create_book(&db, frb_book(true).into())
        .await
        .expect("an owned book must be created through the FFI conversion");
    let book_id = created.id.expect("created book has an id");

    assert_eq!(created.owned, Some(true));
    let copies = copies_of(&db, &book_id).await;
    assert_eq!(copies.len(), 1, "an owned book gets exactly one copy");
    assert_eq!(copies[0].status, "available");
    assert!(!copies[0].is_temporary);
}

#[tokio::test]
async fn unowned_frb_book_gets_no_copy() {
    let db = setup_test_db().await;

    let created = book_service::create_book(&db, frb_book(false).into())
        .await
        .expect("a wished-for book must be created");
    let book_id = created.id.expect("created book has an id");

    assert_eq!(created.owned, Some(false));
    assert!(copies_of(&db, &book_id).await.is_empty());
}
