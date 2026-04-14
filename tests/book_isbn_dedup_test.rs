//! Regression tests for migration 057 (book ISBN deduplication).
//!
//! Bug: books created without an ISBN were stored with `isbn = ''`. The
//! dedup query's `isbn IS NOT NULL` predicate is TRUE for empty strings in
//! SQLite, so every ISBN-less book after the first was deleted on every app
//! startup — silently (bypassing the operation_log) and without a matching
//! business reason (ISBN is optional for self-published / ancient / rare
//! books). See the commit that introduced this test.

use rust_lib_app::db;
use rust_lib_app::models::book;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

async fn insert_book(db: &sea_orm::DatabaseConnection, title: &str, isbn: Option<&str>) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let active = book::ActiveModel {
        title: Set(title.to_string()),
        isbn: Set(isbn.map(|s| s.to_string())),
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

/// Two ISBN-less books must both survive a re-run of migrations. Before the
/// fix, migration 057 treated `isbn = ''` as "same ISBN" and deleted all but
/// the lowest-id row, causing self-published / rare books to disappear.
#[tokio::test]
async fn migrations_keep_both_books_with_empty_string_isbn() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");

    let id_a = insert_book(&db, "Les mouches", Some("")).await;
    let id_b = insert_book(&db, "Journal intime", Some("")).await;

    db::run_migrations(&db).await.expect("re-run migrations");

    let survivors = book::Entity::find()
        .filter(book::Column::Id.is_in([id_a, id_b]))
        .all(&db)
        .await
        .expect("find books");

    assert_eq!(
        survivors.len(),
        2,
        "both ISBN-less books must survive dedup (got ids: {:?})",
        survivors.iter().map(|b| b.id).collect::<Vec<_>>()
    );
}

/// A NULL ISBN and an empty-string ISBN must coexist without either being
/// deleted. Both mean "no ISBN" and must be treated the same.
#[tokio::test]
async fn migrations_keep_null_isbn_and_empty_isbn_together() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");

    let id_null = insert_book(&db, "Book NULL", None).await;
    let id_empty = insert_book(&db, "Book EMPTY", Some("")).await;

    db::run_migrations(&db).await.expect("re-run migrations");

    let survivors = book::Entity::find()
        .filter(book::Column::Id.is_in([id_null, id_empty]))
        .all(&db)
        .await
        .expect("find books");

    assert_eq!(
        survivors.len(),
        2,
        "NULL-ISBN and empty-ISBN books must both survive"
    );
}

/// Real-ISBN duplicates must still be collapsed (the original intent of the
/// migration). Regression guard so we don't over-correct.
#[tokio::test]
async fn migrations_still_dedupe_real_isbn_duplicates() {
    let db = db::init_db("sqlite::memory:").await.expect("init db");

    let keep = insert_book(&db, "Original", Some("9782070368228")).await;
    let dupe = insert_book(&db, "Duplicate", Some("9782070368228")).await;

    db::run_migrations(&db).await.expect("re-run migrations");

    let kept = book::Entity::find_by_id(keep).one(&db).await.unwrap();
    let deleted = book::Entity::find_by_id(dupe).one(&db).await.unwrap();

    assert!(kept.is_some(), "oldest duplicate by ISBN must be kept");
    assert!(deleted.is_none(), "newer duplicate by ISBN must be removed");
}
