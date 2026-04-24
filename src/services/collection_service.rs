//! Collection-level orchestration service.
//!
//! Thin wrapper on top of [`CollectionRepository`] that adds cross-entity
//! business logic. Today it handles "delete a collection, optionally along
//! with the books it contains": a single operation that has to reason
//! about loans, tags (shelves) and overlap with other collections, and
//! needs transactional guarantees.

use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    TransactionTrait,
};

use crate::models::{book, book_tags, collection, collection_book, copy};

#[derive(Debug)]
pub enum CollectionServiceError {
    NotFound,
    Database(String),
}

impl std::fmt::Display for CollectionServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectionServiceError::NotFound => write!(f, "Collection not found"),
            CollectionServiceError::Database(msg) => write!(f, "Database error: {msg}"),
        }
    }
}

impl std::error::Error for CollectionServiceError {}

impl From<sea_orm::DbErr> for CollectionServiceError {
    fn from(e: sea_orm::DbErr) -> Self {
        CollectionServiceError::Database(e.to_string())
    }
}

/// Counts shown in the delete-with-books preview.
///
/// * `total_books` - books currently in the collection
/// * `to_delete` - books that would be deleted (no blocking ties)
/// * `to_keep` - books that would stay (loaned, borrowed, in another
///   collection, or on a shelf)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DeletionPreview {
    pub total_books: i64,
    pub to_delete: i64,
    pub to_keep: i64,
}

/// Compute the per-collection deletion preview.
pub async fn preview_deletion(
    db: &DatabaseConnection,
    collection_id: &str,
) -> Result<DeletionPreview, CollectionServiceError> {
    if collection::Entity::find_by_id(collection_id)
        .one(db)
        .await?
        .is_none()
    {
        return Err(CollectionServiceError::NotFound);
    }

    let book_ids = book_ids_in_collection(db, collection_id).await?;
    let total = book_ids.len() as i64;

    let mut to_delete = 0i64;
    for book_id in &book_ids {
        if is_book_eligible_for_deletion(db, *book_id, collection_id).await? {
            to_delete += 1;
        }
    }

    Ok(DeletionPreview {
        total_books: total,
        to_delete,
        to_keep: total - to_delete,
    })
}

/// Delete a collection. When `delete_books` is true, also delete every book
/// in the collection that (a) has no loaned or borrowed copy, (b) does not
/// belong to another collection, (c) is on no shelf (no `book_tags` row).
///
/// Atomic: book deletions and the collection deletion run inside a single
/// transaction. On any database error the whole operation rolls back.
///
/// Returns the list of book IDs that were actually deleted (empty when
/// `delete_books` is false).
pub async fn delete_collection(
    db: &DatabaseConnection,
    collection_id: &str,
    delete_books: bool,
) -> Result<Vec<i32>, CollectionServiceError> {
    if collection::Entity::find_by_id(collection_id)
        .one(db)
        .await?
        .is_none()
    {
        return Err(CollectionServiceError::NotFound);
    }

    let txn = db.begin().await?;

    let mut deleted_ids: Vec<i32> = Vec::new();
    if delete_books {
        let book_ids = book_ids_in_collection(&txn, collection_id).await?;
        for book_id in book_ids {
            if is_book_eligible_for_deletion(&txn, book_id, collection_id).await? {
                book::Entity::delete_by_id(book_id).exec(&txn).await?;
                deleted_ids.push(book_id);
            }
        }
    }

    let result = collection::Entity::delete_by_id(collection_id)
        .exec(&txn)
        .await?;
    if result.rows_affected == 0 {
        // Concurrent deletion: roll back the book deletes to stay consistent.
        txn.rollback().await.ok();
        return Err(CollectionServiceError::NotFound);
    }

    txn.commit().await?;

    // Post-commit side effects (best-effort, non-critical).
    for id in &deleted_ids {
        let _ = crate::sync::log_operation(db, "book", *id, "DELETE", None).await;
    }
    let _ = crate::sync::log_operation_with_str_id(db, "collection", collection_id, "DELETE", None)
        .await;

    let hub_svc = crate::services::hub_directory_service::HubDirectoryService::new();
    for id in &deleted_ids {
        if let Err(e) = hub_svc.delete_cover(db, *id).await {
            tracing::debug!("hub cover cleanup skipped for book {id}: {e}");
        }
    }

    Ok(deleted_ids)
}

// ── Helpers ──────────────────────────────────────────────────────────────

async fn book_ids_in_collection<C: ConnectionTrait>(
    db: &C,
    collection_id: &str,
) -> Result<Vec<i32>, CollectionServiceError> {
    let rows = collection_book::Entity::find()
        .filter(collection_book::Column::CollectionId.eq(collection_id))
        .all(db)
        .await?;
    Ok(rows.into_iter().map(|r| r.book_id).collect())
}

async fn is_book_eligible_for_deletion<C: ConnectionTrait>(
    db: &C,
    book_id: i32,
    collection_id: &str,
) -> Result<bool, CollectionServiceError> {
    let active_copies = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.is_in(["loaned", "borrowed"]))
        .count(db)
        .await?;
    if active_copies > 0 {
        return Ok(false);
    }

    let other_collections = collection_book::Entity::find()
        .filter(collection_book::Column::BookId.eq(book_id))
        .filter(collection_book::Column::CollectionId.ne(collection_id))
        .count(db)
        .await?;
    if other_collections > 0 {
        return Ok(false);
    }

    let tags = book_tags::Entity::find()
        .filter(book_tags::Column::BookId.eq(book_id))
        .count(db)
        .await?;
    if tags > 0 {
        return Ok(false);
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Set, Statement};

    async fn setup_db() -> DatabaseConnection {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        // The real library_id FK is not exercised in these tests; turn the
        // FK checks off so we can seed copies with `library_id = 0` just
        // like `book_service::tests` does.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();
        db
    }

    async fn insert_book(db: &DatabaseConnection, title: &str) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        book::Entity::insert(book::ActiveModel {
            title: Set(title.to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap()
        .last_insert_id
    }

    async fn insert_collection(db: &DatabaseConnection, id: &str, name: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        collection::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(name.to_owned()),
            description: Set(None),
            source: Set("manual".to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn attach_book(db: &DatabaseConnection, collection_id: &str, book_id: i32) {
        collection_book::ActiveModel {
            collection_id: Set(collection_id.to_owned()),
            book_id: Set(book_id),
            added_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn insert_copy(db: &DatabaseConnection, book_id: i32, status: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        copy::Entity::insert(copy::ActiveModel {
            book_id: Set(book_id),
            library_id: Set(0),
            status: Set(status.to_owned()),
            is_temporary: Set(status == "borrowed"),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
    }

    async fn insert_tag(db: &DatabaseConnection, id: i32, name: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        crate::models::tag::ActiveModel {
            id: Set(id),
            name: Set(name.to_owned()),
            parent_id: Set(None),
            path: Set(name.to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn attach_tag(db: &DatabaseConnection, book_id: i32, tag_id: i32) {
        book_tags::ActiveModel {
            book_id: Set(book_id),
            tag_id: Set(tag_id),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn book_exists(db: &DatabaseConnection, id: i32) -> bool {
        book::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .is_some()
    }

    async fn collection_exists(db: &DatabaseConnection, id: &str) -> bool {
        collection::Entity::find_by_id(id)
            .one(db)
            .await
            .unwrap()
            .is_some()
    }

    #[tokio::test]
    async fn preview_returns_not_found_for_unknown_collection() {
        let db = setup_db().await;
        let err = preview_deletion(&db, "ghost").await.unwrap_err();
        assert!(matches!(err, CollectionServiceError::NotFound));
    }

    #[tokio::test]
    async fn preview_all_books_eligible_when_no_blockers() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "Reading list").await;
        let b1 = insert_book(&db, "A").await;
        let b2 = insert_book(&db, "B").await;
        let b3 = insert_book(&db, "C").await;
        attach_book(&db, "c1", b1).await;
        attach_book(&db, "c1", b2).await;
        attach_book(&db, "c1", b3).await;

        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(
            preview,
            DeletionPreview {
                total_books: 3,
                to_delete: 3,
                to_keep: 0,
            }
        );
    }

    #[tokio::test]
    async fn preview_counts_loaned_as_kept() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        let loaned = insert_book(&db, "lent").await;
        let free = insert_book(&db, "free").await;
        attach_book(&db, "c1", loaned).await;
        attach_book(&db, "c1", free).await;
        insert_copy(&db, loaned, "loaned").await;
        insert_copy(&db, free, "available").await;

        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(preview.total_books, 2);
        assert_eq!(preview.to_delete, 1);
        assert_eq!(preview.to_keep, 1);
    }

    #[tokio::test]
    async fn preview_counts_borrowed_as_kept() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        let borrowed = insert_book(&db, "borrowed").await;
        attach_book(&db, "c1", borrowed).await;
        insert_copy(&db, borrowed, "borrowed").await;

        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(preview.to_keep, 1);
        assert_eq!(preview.to_delete, 0);
    }

    #[tokio::test]
    async fn preview_counts_multi_collection_books_as_kept() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        insert_collection(&db, "c2", "c2").await;
        let shared = insert_book(&db, "shared").await;
        let solo = insert_book(&db, "solo").await;
        attach_book(&db, "c1", shared).await;
        attach_book(&db, "c2", shared).await;
        attach_book(&db, "c1", solo).await;

        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(preview.to_delete, 1);
        assert_eq!(preview.to_keep, 1);
    }

    #[tokio::test]
    async fn preview_counts_shelved_books_as_kept() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        let shelved = insert_book(&db, "on shelf").await;
        let free = insert_book(&db, "free").await;
        attach_book(&db, "c1", shelved).await;
        attach_book(&db, "c1", free).await;
        insert_tag(&db, 1, "fiction").await;
        attach_tag(&db, shelved, 1).await;

        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(preview.to_delete, 1);
        assert_eq!(preview.to_keep, 1);
    }

    #[tokio::test]
    async fn preview_counts_multi_blocker_book_once() {
        // A book blocked for several reasons at once (loaned AND on a shelf)
        // must still count as a single "kept" entry so totals add up.
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        let blocked = insert_book(&db, "blocked").await;
        attach_book(&db, "c1", blocked).await;
        insert_copy(&db, blocked, "loaned").await;
        insert_tag(&db, 1, "fiction").await;
        attach_tag(&db, blocked, 1).await;

        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(preview.total_books, 1);
        assert_eq!(preview.to_delete, 0);
        assert_eq!(preview.to_keep, 1);
    }

    #[tokio::test]
    async fn preview_on_empty_collection_returns_zeroes() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "empty").await;
        let preview = preview_deletion(&db, "c1").await.unwrap();
        assert_eq!(
            preview,
            DeletionPreview {
                total_books: 0,
                to_delete: 0,
                to_keep: 0,
            }
        );
    }

    #[tokio::test]
    async fn delete_without_flag_keeps_books() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        let b1 = insert_book(&db, "keep me").await;
        attach_book(&db, "c1", b1).await;

        let deleted = delete_collection(&db, "c1", false).await.unwrap();

        assert!(deleted.is_empty(), "flag=false must not delete any book");
        assert!(!collection_exists(&db, "c1").await);
        assert!(book_exists(&db, b1).await, "book must remain orphaned");
    }

    #[tokio::test]
    async fn delete_with_flag_deletes_eligible_only() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;
        insert_collection(&db, "c2", "c2").await;

        let eligible = insert_book(&db, "eligible").await;
        let loaned = insert_book(&db, "loaned").await;
        let in_c2 = insert_book(&db, "in_c2").await;
        let shelved = insert_book(&db, "shelved").await;
        attach_book(&db, "c1", eligible).await;
        attach_book(&db, "c1", loaned).await;
        attach_book(&db, "c1", in_c2).await;
        attach_book(&db, "c2", in_c2).await;
        attach_book(&db, "c1", shelved).await;
        insert_copy(&db, loaned, "loaned").await;
        insert_tag(&db, 1, "fiction").await;
        attach_tag(&db, shelved, 1).await;

        let deleted = delete_collection(&db, "c1", true).await.unwrap();
        assert_eq!(deleted, vec![eligible]);

        assert!(!collection_exists(&db, "c1").await);
        assert!(collection_exists(&db, "c2").await, "c2 must not be touched");

        assert!(!book_exists(&db, eligible).await);
        assert!(book_exists(&db, loaned).await);
        assert!(book_exists(&db, in_c2).await);
        assert!(book_exists(&db, shelved).await);
    }

    #[tokio::test]
    async fn delete_with_flag_on_empty_collection_just_drops_it() {
        let db = setup_db().await;
        insert_collection(&db, "c1", "c1").await;

        let deleted = delete_collection(&db, "c1", true).await.unwrap();
        assert!(deleted.is_empty());
        assert!(!collection_exists(&db, "c1").await);
    }

    #[tokio::test]
    async fn delete_unknown_collection_returns_not_found() {
        let db = setup_db().await;
        let err = delete_collection(&db, "ghost", false).await.unwrap_err();
        assert!(matches!(err, CollectionServiceError::NotFound));

        let err2 = delete_collection(&db, "ghost", true).await.unwrap_err();
        assert!(matches!(err2, CollectionServiceError::NotFound));
    }
}
