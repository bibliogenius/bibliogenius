//! Bulk filing of books onto shelves and into collections.
//!
//! One entry point, [`assign_books`], shared by the FFI and the HTTP handler:
//! a selection of books gains shelves and/or collections, and optionally
//! leaves the shelf or collection it was selected from. Everything commits in
//! ONE transaction, so a failure halfway never leaves a selection half filed.
//!
//! Shelf membership lives in `books.subjects` (a JSON array of shelf paths),
//! collection membership in the `collection_books` junction.

use std::collections::HashSet;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    Set, TransactionTrait,
};

use crate::models::book::{ActiveModel as BookActiveModel, Entity as BookEntity};
use crate::models::{book, collection, collection_book};
use crate::services::book_service::ServiceError;

/// Upper bound on one request. Far above any hand-made selection ("select
/// all" on a large library), low enough to keep one transaction bounded.
pub const MAX_BOOKS_PER_ASSIGNMENT: usize = 5000;

/// Longest accepted shelf path, in characters.
const MAX_SHELF_NAME_CHARS: usize = 200;

/// Ids per `IN (...)` query, under SQLite's bound-parameter limit.
const ID_CHUNK_SIZE: usize = 500;

/// What to do with a selection of books. Additions are idempotent; removals
/// are exact matches. A name or id present on both sides is kept (the
/// addition wins), so "move to the shelf it is already on" is a no-op.
#[derive(Debug, Default, Clone)]
pub struct BulkAssignment {
    pub book_ids: Vec<String>,
    /// Shelf paths (e.g. `"Genre > Roman"`) to append to each book's subjects.
    pub add_shelves: Vec<String>,
    pub add_collection_ids: Vec<String>,
    /// Shelf paths to strip from each book's subjects.
    pub remove_shelves: Vec<String>,
    pub remove_collection_ids: Vec<String>,
}

/// Number of selected books whose shelves or collections actually changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkAssignmentOutcome {
    pub books_changed: usize,
}

/// Apply `assignment` atomically.
///
/// Unknown book ids are ignored (the selection may be stale); an unknown
/// collection id is an error and rolls everything back, since silently
/// dropping the destination would report a success that filed nothing.
pub async fn assign_books(
    db: &DatabaseConnection,
    assignment: BulkAssignment,
) -> Result<BulkAssignmentOutcome, ServiceError> {
    let book_ids = dedup(assignment.book_ids);
    if book_ids.len() > MAX_BOOKS_PER_ASSIGNMENT {
        return Err(ServiceError::InvalidInput(format!(
            "at most {MAX_BOOKS_PER_ASSIGNMENT} books per request"
        )));
    }
    let add_shelves = clean_shelf_names(assignment.add_shelves)?;
    let add_collections = dedup(assignment.add_collection_ids);
    let remove_shelves: Vec<String> = clean_shelf_names(assignment.remove_shelves)?
        .into_iter()
        .filter(|name| !add_shelves.contains(name))
        .collect();
    let remove_collections: Vec<String> = dedup(assignment.remove_collection_ids)
        .into_iter()
        .filter(|id| !add_collections.contains(id))
        .collect();

    if book_ids.is_empty() {
        return Ok(BulkAssignmentOutcome { books_changed: 0 });
    }

    let txn = db.begin().await?;

    for collection_id in &add_collections {
        if collection::Entity::find_by_id(collection_id.clone())
            .one(&txn)
            .await?
            .is_none()
        {
            txn.rollback().await.ok();
            return Err(ServiceError::NotFound);
        }
    }

    let mut changed: HashSet<String> = HashSet::new();
    let now = chrono::Utc::now().to_rfc3339();

    for chunk in book_ids.chunks(ID_CHUNK_SIZE) {
        let books = BookEntity::find()
            .filter(book::Column::Id.is_in(chunk.iter().cloned()))
            .all(&txn)
            .await?;
        let existing_ids: Vec<String> = books.iter().map(|b| b.id.clone()).collect();

        if !add_shelves.is_empty() || !remove_shelves.is_empty() {
            for book in books {
                let Some(subjects) =
                    refiled_subjects(book.subjects.as_deref(), &add_shelves, &remove_shelves)
                else {
                    continue;
                };
                let id = book.id.clone();
                let mut active: BookActiveModel = book.into();
                active.subjects = Set(subjects);
                active.updated_at = Set(now.clone());
                active.update(&txn).await?;
                changed.insert(id);
            }
        }

        for collection_id in &add_collections {
            let already: HashSet<String> = collection_book::Entity::find()
                .filter(collection_book::Column::CollectionId.eq(collection_id.as_str()))
                .filter(collection_book::Column::BookId.is_in(existing_ids.iter().cloned()))
                .all(&txn)
                .await?
                .into_iter()
                .map(|link| link.book_id)
                .collect();
            for book_id in existing_ids.iter().filter(|id| !already.contains(*id)) {
                // Unnumbered, like a single add: a volume number is assigned
                // separately for series-typed collections.
                collection_book::ActiveModel {
                    collection_id: Set(collection_id.clone()),
                    book_id: Set(book_id.clone()),
                    added_at: Set(now.clone()),
                    volume_number: Set(None),
                }
                .insert(&txn)
                .await?;
                changed.insert(book_id.clone());
            }
        }

        for collection_id in &remove_collections {
            changed.extend(unlink_from_collection(&txn, collection_id, &existing_ids).await?);
        }
    }

    txn.commit().await?;
    Ok(BulkAssignmentOutcome {
        books_changed: changed.len(),
    })
}

/// Remove the links between `collection_id` and `book_ids`, returning the
/// books that were actually members.
async fn unlink_from_collection<C: ConnectionTrait>(
    conn: &C,
    collection_id: &str,
    book_ids: &[String],
) -> Result<Vec<String>, ServiceError> {
    let members: Vec<String> = collection_book::Entity::find()
        .filter(collection_book::Column::CollectionId.eq(collection_id))
        .filter(collection_book::Column::BookId.is_in(book_ids.iter().cloned()))
        .all(conn)
        .await?
        .into_iter()
        .map(|link| link.book_id)
        .collect();
    if !members.is_empty() {
        collection_book::Entity::delete_many()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .filter(collection_book::Column::BookId.is_in(members.iter().cloned()))
            .exec(conn)
            .await?;
    }
    Ok(members)
}

/// The subjects column after adding and removing shelves, or `None` when the
/// book is already in the requested state and must not be rewritten.
///
/// A column that does not parse as a JSON string array is left alone when
/// there is nothing to add, and replaced by the additions otherwise: the
/// listing already ignores such a value, so nothing readable is lost.
fn refiled_subjects(
    current: Option<&str>,
    add: &[String],
    remove: &[String],
) -> Option<Option<String>> {
    let parsed = current.and_then(|json| serde_json::from_str::<Vec<String>>(json).ok());
    let before = parsed.clone().unwrap_or_default();

    let mut after: Vec<String> = before
        .iter()
        .filter(|s| !remove.contains(s))
        .cloned()
        .collect();
    for name in add {
        if !after.contains(name) {
            after.push(name.clone());
        }
    }

    if after == before {
        return None;
    }
    if after.is_empty() {
        // Same spelling of "no shelf" as a book that never had one.
        return Some(None);
    }
    Some(Some(serde_json::to_string(&after).unwrap_or_default()))
}

/// Trim, drop duplicates, and refuse blank or oversized shelf names.
fn clean_shelf_names(names: Vec<String>) -> Result<Vec<String>, ServiceError> {
    let mut cleaned: Vec<String> = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            return Err(ServiceError::InvalidInput(
                "a shelf name cannot be empty".to_string(),
            ));
        }
        if name.chars().count() > MAX_SHELF_NAME_CHARS {
            return Err(ServiceError::InvalidInput(format!(
                "a shelf name cannot exceed {MAX_SHELF_NAME_CHARS} characters"
            )));
        }
        if !cleaned.iter().any(|c| c == name) {
            cleaned.push(name.to_owned());
        }
    }
    Ok(cleaned)
}

fn dedup(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_db() -> DatabaseConnection {
        crate::db::init_db("sqlite::memory:").await.unwrap()
    }

    async fn insert_book(db: &DatabaseConnection, title: &str, subjects: Option<&str>) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        // `Entity::insert(...).exec()` does not run `before_save`, so the
        // uuid primary key is set explicitly.
        let id = crate::utils::uuid_gen::new_uuid_v7();
        book::Entity::insert(book::ActiveModel {
            id: Set(id.clone()),
            title: Set(title.to_owned()),
            subjects: Set(subjects.map(str::to_owned)),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        id
    }

    async fn insert_collection(db: &DatabaseConnection, name: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let id = crate::utils::uuid_gen::new_uuid_v7();
        collection::ActiveModel {
            id: Set(id.clone()),
            name: Set(name.to_owned()),
            description: Set(None),
            source: Set("manual".to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
        id
    }

    async fn subjects_of(db: &DatabaseConnection, id: &str) -> Option<String> {
        BookEntity::find_by_id(id.to_owned())
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .subjects
    }

    async fn members_of(db: &DatabaseConnection, collection_id: &str) -> HashSet<String> {
        collection_book::Entity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|link| link.book_id)
            .collect()
    }

    #[tokio::test]
    async fn adding_a_shelf_appends_it_and_keeps_the_existing_ones() {
        let db = setup_db().await;
        let bare = insert_book(&db, "No shelf", None).await;
        let filed = insert_book(&db, "Already filed", Some(r#"["Polar"]"#)).await;
        let untouched = insert_book(&db, "Not selected", Some(r#"["Polar"]"#)).await;

        let outcome = assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![bare.clone(), filed.clone()],
                add_shelves: vec!["Genre > Roman".to_owned()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.books_changed, 2);
        assert_eq!(
            subjects_of(&db, &bare).await.as_deref(),
            Some(r#"["Genre > Roman"]"#)
        );
        assert_eq!(
            subjects_of(&db, &filed).await.as_deref(),
            Some(r#"["Polar","Genre > Roman"]"#)
        );
        assert_eq!(
            subjects_of(&db, &untouched).await.as_deref(),
            Some(r#"["Polar"]"#)
        );
    }

    #[tokio::test]
    async fn a_book_already_on_the_shelf_is_not_rewritten() {
        let db = setup_db().await;
        let filed = insert_book(&db, "Already there", Some(r#"["Roman"]"#)).await;
        let before = BookEntity::find_by_id(filed.clone())
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .updated_at;

        let outcome = assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![filed.clone(), filed.clone()],
                add_shelves: vec!["Roman".to_owned(), " Roman ".to_owned()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.books_changed, 0);
        assert_eq!(
            subjects_of(&db, &filed).await.as_deref(),
            Some(r#"["Roman"]"#)
        );
        let after = BookEntity::find_by_id(filed)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .updated_at;
        assert_eq!(before, after, "an unchanged book must not replicate");
    }

    #[tokio::test]
    async fn moving_strips_the_source_shelf_by_exact_match() {
        let db = setup_db().await;
        let moved = insert_book(&db, "Moved", Some(r#"["Roman","Roman classique"]"#)).await;
        let emptied = insert_book(&db, "Only removed", Some(r#"["Roman"]"#)).await;

        assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![moved.clone()],
                add_shelves: vec!["Polar".to_owned()],
                remove_shelves: vec!["Roman".to_owned()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![emptied.clone()],
                remove_shelves: vec!["Roman".to_owned()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(
            subjects_of(&db, &moved).await.as_deref(),
            Some(r#"["Roman classique","Polar"]"#)
        );
        assert_eq!(subjects_of(&db, &emptied).await, None);
    }

    #[tokio::test]
    async fn moving_to_the_source_itself_keeps_the_books_there() {
        let db = setup_db().await;
        let book = insert_book(&db, "Stays", Some(r#"["Roman"]"#)).await;
        let collection = insert_collection(&db, "Series").await;
        assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![book.clone()],
                add_collection_ids: vec![collection.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let outcome = assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![book.clone()],
                add_shelves: vec!["Roman".to_owned()],
                remove_shelves: vec!["Roman".to_owned()],
                add_collection_ids: vec![collection.clone()],
                remove_collection_ids: vec![collection.clone()],
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.books_changed, 0);
        assert_eq!(
            subjects_of(&db, &book).await.as_deref(),
            Some(r#"["Roman"]"#)
        );
        assert!(members_of(&db, &collection).await.contains(&book));
    }

    #[tokio::test]
    async fn collections_gain_and_lose_members_idempotently() {
        let db = setup_db().await;
        let a = insert_book(&db, "A", None).await;
        let b = insert_book(&db, "B", None).await;
        let source = insert_collection(&db, "Source").await;
        let target = insert_collection(&db, "Target").await;
        for collection_id in [&source, &target] {
            // `a` is already in both; `b` only in the source.
            assign_books(
                &db,
                BulkAssignment {
                    book_ids: vec![a.clone()],
                    add_collection_ids: vec![collection_id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![b.clone()],
                add_collection_ids: vec![source.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let outcome = assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![a.clone(), b.clone(), "unknown-book".to_owned()],
                add_collection_ids: vec![target.clone()],
                remove_collection_ids: vec![source.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.books_changed, 2);
        assert_eq!(
            members_of(&db, &target).await,
            HashSet::from([a.clone(), b.clone()]),
            "an unknown book id never becomes an orphan link"
        );
        assert!(members_of(&db, &source).await.is_empty());
    }

    #[tokio::test]
    async fn an_unknown_collection_rolls_the_whole_assignment_back() {
        let db = setup_db().await;
        let book = insert_book(&db, "Selected", None).await;

        let result = assign_books(
            &db,
            BulkAssignment {
                book_ids: vec![book.clone()],
                add_shelves: vec!["Roman".to_owned()],
                add_collection_ids: vec!["00000000-0000-0000-0000-000000000000".to_owned()],
                ..Default::default()
            },
        )
        .await;

        assert!(matches!(result, Err(ServiceError::NotFound)));
        assert_eq!(subjects_of(&db, &book).await, None);
    }

    #[tokio::test]
    async fn blank_or_oversized_shelf_names_are_refused() {
        let db = setup_db().await;
        let book = insert_book(&db, "Selected", None).await;
        for name in ["   ".to_owned(), "x".repeat(MAX_SHELF_NAME_CHARS + 1)] {
            let result = assign_books(
                &db,
                BulkAssignment {
                    book_ids: vec![book.clone()],
                    add_shelves: vec![name],
                    ..Default::default()
                },
            )
            .await;
            assert!(matches!(result, Err(ServiceError::InvalidInput(_))));
        }
        assert_eq!(subjects_of(&db, &book).await, None);
    }

    #[test]
    fn an_unparseable_subjects_column_is_only_replaced_when_adding() {
        assert_eq!(
            refiled_subjects(Some("not json"), &[], &["Roman".to_owned()]),
            None
        );
        assert_eq!(
            refiled_subjects(Some("not json"), &["Roman".to_owned()], &[]),
            Some(Some(r#"["Roman"]"#.to_owned()))
        );
    }
}
