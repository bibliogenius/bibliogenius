//! A book cannot be created without a title.
//!
//! A title-less book is invisible in every list that identifies books by
//! title, and it reaches peers as a blank entry. The guard lives in
//! `book_service::create_book`, next to the reading-status gate, which is the
//! single door every user-facing creation goes through (FFI and the HTTP
//! handler both call the shared validator).
//!
//! Deliberately NOT guarded: cr-sqlite account-sync replication and peer
//! caches, which insert through the entity directly. A replicated row must
//! land as-is, even when it is malformed: refusing it would desynchronise the
//! devices instead of surfacing the problem.

use rust_lib_app::db;
use rust_lib_app::models::Book;
use rust_lib_app::services::book_service::{self, ServiceError};
use sea_orm::DatabaseConnection;

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

fn book_with_title(title: &str) -> Book {
    Book {
        title: title.to_string(),
        isbn: Some("9782070612918".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn create_rejects_an_empty_title() {
    let db = setup_test_db().await;

    let err = book_service::create_book(&db, book_with_title(""))
        .await
        .expect_err("an empty title must be refused");

    assert!(
        matches!(err, ServiceError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}",
    );
}

#[tokio::test]
async fn create_rejects_a_whitespace_only_title() {
    let db = setup_test_db().await;

    // "   " passes a naive is_empty() check but leaves the same blank tile.
    let err = book_service::create_book(&db, book_with_title("   \t "))
        .await
        .expect_err("a blank title must be refused");

    assert!(
        matches!(err, ServiceError::InvalidInput(_)),
        "expected InvalidInput, got {err:?}",
    );
}

#[tokio::test]
async fn create_accepts_a_real_title() {
    let db = setup_test_db().await;

    let created = book_service::create_book(&db, book_with_title("Le Mythe de Sisyphe"))
        .await
        .expect("a titled book is created");

    assert_eq!(created.title, "Le Mythe de Sisyphe");
}

#[tokio::test]
async fn create_keeps_surrounding_whitespace_out_of_the_stored_title() {
    let db = setup_test_db().await;

    let created = book_service::create_book(&db, book_with_title("  Nadja  "))
        .await
        .expect("a titled book is created");

    assert_eq!(
        created.title, "Nadja",
        "a title typed with stray spaces must be stored trimmed, so it sorts \
         and matches like any other",
    );
}
