//! `POST /books/batch/edit` with the `Delete` action must remove each book the
//! way a single delete does: copies, their loans and sales, author/tag/
//! collection links and notes go with the book, and one operation-log row
//! records every deletion. The replicated tables carry no foreign keys since
//! ADR-044, so nothing cascades in the database itself.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rust_lib_app::api;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{
    book, book_authors, book_tags, collection_book, copy, loan, operation_log, sale,
};
use rust_lib_app::modules::book_notes::models as book_note;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Set, Statement,
};
use tower::ServiceExt;

async fn setup_db() -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    // The seeded loan points at a contact that does not exist; the replicated
    // tables have no foreign keys in production either (ADR-044).
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "PRAGMA foreign_keys = OFF".to_owned(),
    ))
    .await
    .unwrap();
    db
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn batch_router(db: &DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route(
            "/books/batch/edit",
            axum::routing::post(api::batch::batch_edit),
        )
        .with_state(AppState::new(db.clone()))
}

/// Seed a book with one of every dependent row and return its id.
async fn seed_full_book(db: &DatabaseConnection, title: &str) -> String {
    let book_id = book::ActiveModel {
        title: Set(title.to_owned()),
        created_at: Set(now()),
        updated_at: Set(now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id;

    let copy_id = copy::ActiveModel {
        book_id: Set(book_id.clone()),
        library_id: Set(0),
        status: Set("available".to_owned()),
        is_temporary: Set(false),
        created_at: Set(now()),
        updated_at: Set(now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id;

    loan::ActiveModel {
        copy_id: Set(copy_id.clone()),
        contact_id: Set("contact-1".to_owned()),
        library_id: Set(0),
        loan_date: Set(now()),
        due_date: Set(now()),
        status: Set("active".to_owned()),
        created_at: Set(now()),
        updated_at: Set(now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    sale::ActiveModel {
        copy_id: Set(copy_id),
        contact_id: Set(None),
        library_id: Set(0),
        sale_date: Set(now()),
        sale_price: Set(10.0),
        status: Set("completed".to_owned()),
        notes: Set(None),
        created_at: Set(now()),
        updated_at: Set(now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    book_authors::ActiveModel {
        book_id: Set(book_id.clone()),
        author_id: Set("author-1".to_owned()),
    }
    .insert(db)
    .await
    .unwrap();

    book_tags::ActiveModel {
        book_id: Set(book_id.clone()),
        tag_id: Set("tag-1".to_owned()),
    }
    .insert(db)
    .await
    .unwrap();

    collection_book::ActiveModel {
        collection_id: Set("collection-1".to_owned()),
        book_id: Set(book_id.clone()),
        added_at: Set(now()),
        volume_number: Set(None),
    }
    .insert(db)
    .await
    .unwrap();

    book_note::ActiveModel {
        book_id: Set(book_id.clone()),
        content: Set("a note".to_owned()),
        page: Set(None),
        created_at: Set(now()),
        updated_at: Set(now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    book_id
}

async fn count<E: EntityTrait>(db: &DatabaseConnection) -> usize {
    E::find().all(db).await.unwrap().len()
}

async fn batch_delete(app: axum::Router, ids: &[&str]) -> (StatusCode, serde_json::Value) {
    let payload = serde_json::json!({ "ids": ids, "action": { "type": "Delete" } });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/books/batch/edit")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn batch_delete_removes_the_books_and_every_dependent_row() {
    let db = setup_db().await;
    let first = seed_full_book(&db, "first").await;
    let second = seed_full_book(&db, "second").await;

    let (status, body) = batch_delete(batch_router(&db), &[&first, &second]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], 2);
    assert_eq!(count::<book::Entity>(&db).await, 0, "books");
    assert_eq!(count::<copy::Entity>(&db).await, 0, "copies");
    assert_eq!(count::<loan::Entity>(&db).await, 0, "loans");
    assert_eq!(count::<sale::Entity>(&db).await, 0, "sales");
    assert_eq!(count::<book_authors::Entity>(&db).await, 0, "book_authors");
    assert_eq!(count::<book_tags::Entity>(&db).await, 0, "book_tags");
    assert_eq!(
        count::<collection_book::Entity>(&db).await,
        0,
        "collection_books"
    );
    assert_eq!(count::<book_note::Entity>(&db).await, 0, "book_notes");
}

#[tokio::test]
async fn batch_delete_leaves_the_other_books_untouched() {
    let db = setup_db().await;
    let doomed = seed_full_book(&db, "doomed").await;
    let survivor = seed_full_book(&db, "survivor").await;

    let (status, body) = batch_delete(batch_router(&db), &[&doomed]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], 1);
    assert!(
        book::Entity::find_by_id(survivor.clone())
            .one(&db)
            .await
            .unwrap()
            .is_some(),
        "survivor"
    );
    // The survivor keeps exactly its own single row in every table.
    assert_eq!(count::<copy::Entity>(&db).await, 1, "copies");
    assert_eq!(count::<loan::Entity>(&db).await, 1, "loans");
    assert_eq!(count::<sale::Entity>(&db).await, 1, "sales");
    assert_eq!(count::<book_authors::Entity>(&db).await, 1, "book_authors");
    assert_eq!(count::<book_tags::Entity>(&db).await, 1, "book_tags");
    assert_eq!(
        count::<collection_book::Entity>(&db).await,
        1,
        "collection_books"
    );
    assert_eq!(count::<book_note::Entity>(&db).await, 1, "book_notes");
}

#[tokio::test]
async fn batch_delete_counts_only_the_books_that_existed() {
    let db = setup_db().await;
    let existing = seed_full_book(&db, "existing").await;
    let survivor = seed_full_book(&db, "survivor").await;

    let (status, body) = batch_delete(batch_router(&db), &[&existing, "does-not-exist"]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], 1);
    assert_eq!(count::<book::Entity>(&db).await, 1, "books");
    assert!(
        book::Entity::find_by_id(survivor)
            .one(&db)
            .await
            .unwrap()
            .is_some(),
        "an unknown id must not touch the other books"
    );
}

#[tokio::test]
async fn batch_delete_logs_one_delete_per_book() {
    let db = setup_db().await;
    let first = seed_full_book(&db, "first").await;
    let second = seed_full_book(&db, "second").await;

    let (status, _) = batch_delete(batch_router(&db), &[&first, &second]).await;
    assert_eq!(status, StatusCode::OK);

    let logged: Vec<String> = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("book"))
        .filter(operation_log::Column::Operation.eq("DELETE"))
        .all(&db)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.entity_id)
        .collect();
    assert_eq!(logged.len(), 2, "one DELETE row per book: {logged:?}");
    assert!(logged.contains(&first));
    assert!(logged.contains(&second));
}
