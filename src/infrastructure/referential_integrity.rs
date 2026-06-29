//! Application-level referential integrity.
//!
//! The replicated entity tables (books, copies, authors, tags, contacts,
//! loans + their junctions) were rebuilt without `FOREIGN KEY` clauses so that
//! their primary key could become a cross-device-stable UUID (ADR-044). That
//! rebuild also removed every `ON DELETE CASCADE` the schema used to rely on,
//! so deleting a parent row no longer removes its children at the database
//! level. If the application does not delete those children explicitly, they
//! linger as orphans locally AND, once a device syncs, the missing child
//! deletes let another device re-introduce the same dangling references.
//!
//! This module is the single source of truth for that cascade behaviour. Each
//! `delete_*_cascade` helper removes a parent and all of its dependent rows in
//! the caller's transaction, mirroring the foreign keys that were dropped.
//! Callers MUST pass a transaction (or a connection they accept as the unit of
//! atomicity): the parent delete and the child deletes have to commit together,
//! both for local consistency and so the per-row deletes propagate as a single
//! coherent change set during sync.

use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, QueryTrait,
};

use crate::models::{book, book_authors, book_tags, collection_book, copy, loan, sale};
use crate::modules::book_notes::models as book_note;

/// Delete a book and every row that referenced it through a foreign key that
/// existed before the UUID-PK rebuild (ADR-044): its copies (and, transitively,
/// the loans and sales of those copies), its author/tag/collection junction
/// rows, and its notes. Idempotent: deleting an unknown book is a no-op.
///
/// Runs in the caller-provided connection so the whole cascade is one atomic
/// unit; pass a transaction.
pub async fn delete_book_cascade<C>(conn: &C, book_uuid: &str) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    // Grandchildren first: loans and sales point at the book's copies, so they
    // must go before the copies do. Match them with a subquery on copies so
    // each delete is a single statement, without a round-trip to collect the
    // copy ids (and an empty book naturally deletes nothing).
    let copies_of_book = copy::Entity::find()
        .select_only()
        .column(copy::Column::Id)
        .filter(copy::Column::BookId.eq(book_uuid))
        .into_query();

    loan::Entity::delete_many()
        .filter(loan::Column::CopyId.in_subquery(copies_of_book.clone()))
        .exec(conn)
        .await?;
    sale::Entity::delete_many()
        .filter(sale::Column::CopyId.in_subquery(copies_of_book))
        .exec(conn)
        .await?;

    // Direct children of the book.
    copy::Entity::delete_many()
        .filter(copy::Column::BookId.eq(book_uuid))
        .exec(conn)
        .await?;
    book_authors::Entity::delete_many()
        .filter(book_authors::Column::BookId.eq(book_uuid))
        .exec(conn)
        .await?;
    book_tags::Entity::delete_many()
        .filter(book_tags::Column::BookId.eq(book_uuid))
        .exec(conn)
        .await?;
    collection_book::Entity::delete_many()
        .filter(collection_book::Column::BookId.eq(book_uuid))
        .exec(conn)
        .await?;
    book_note::Entity::delete_many()
        .filter(book_note::Column::BookId.eq(book_uuid))
        .exec(conn)
        .await?;

    // Finally the book row itself.
    book::Entity::delete_by_id(book_uuid.to_owned())
        .exec(conn)
        .await?;

    Ok(())
}

/// Delete every `collection_books` junction row for a collection. Mirrors the
/// `collection -> collection_books` cascade dropped by the UUID-PK rebuild.
/// Removes the links for books that stay in the library as well as for books
/// the caller deleted; the books themselves are untouched.
pub async fn delete_collection_links<C>(conn: &C, collection_id: &str) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
    collection_book::Entity::delete_many()
        .filter(collection_book::Column::CollectionId.eq(collection_id))
        .exec(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, Statement};

    async fn setup_db() -> DatabaseConnection {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        // The replicated tables no longer carry foreign keys, but the
        // first-launch schema still creates them before `migrate_uuid_pk`
        // rebuilds them. Turn FK enforcement off so fixtures can use
        // `library_id = 0` and synthetic contact/author/tag ids, exactly like
        // `collection_service::tests` and `book_service::tests`.
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

    async fn insert_book(db: &DatabaseConnection, title: &str) -> String {
        let id = crate::utils::uuid_gen::new_uuid_v7();
        book::Entity::insert(book::ActiveModel {
            id: Set(id.clone()),
            title: Set(title.to_owned()),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        id
    }

    async fn insert_copy(db: &DatabaseConnection, book_id: &str) -> String {
        let id = crate::utils::uuid_gen::new_uuid_v7();
        copy::Entity::insert(copy::ActiveModel {
            id: Set(id.clone()),
            book_id: Set(book_id.to_owned()),
            library_id: Set(0),
            status: Set("available".to_owned()),
            is_temporary: Set(false),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        id
    }

    async fn insert_loan(db: &DatabaseConnection, copy_id: &str) {
        loan::Entity::insert(loan::ActiveModel {
            id: Set(crate::utils::uuid_gen::new_uuid_v7()),
            copy_id: Set(copy_id.to_owned()),
            contact_id: Set("contact-1".to_owned()),
            library_id: Set(0),
            loan_date: Set(now()),
            due_date: Set(now()),
            status: Set("active".to_owned()),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
    }

    async fn insert_sale(db: &DatabaseConnection, copy_id: &str) {
        sale::ActiveModel {
            copy_id: Set(copy_id.to_owned()),
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
    }

    async fn attach_author(db: &DatabaseConnection, book_id: &str, author_id: &str) {
        book_authors::ActiveModel {
            book_id: Set(book_id.to_owned()),
            author_id: Set(author_id.to_owned()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn attach_tag(db: &DatabaseConnection, book_id: &str, tag_id: &str) {
        book_tags::ActiveModel {
            book_id: Set(book_id.to_owned()),
            tag_id: Set(tag_id.to_owned()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn attach_collection(db: &DatabaseConnection, collection_id: &str, book_id: &str) {
        collection_book::ActiveModel {
            collection_id: Set(collection_id.to_owned()),
            book_id: Set(book_id.to_owned()),
            added_at: Set(now()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn attach_note(db: &DatabaseConnection, book_id: &str) {
        book_note::ActiveModel {
            book_id: Set(book_id.to_owned()),
            content: Set("a note".to_owned()),
            page: Set(None),
            created_at: Set(now()),
            updated_at: Set(now()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// Seed a book with one of every dependent row, returning (book, copy).
    async fn seed_full_book(db: &DatabaseConnection, title: &str) -> (String, String) {
        let book_id = insert_book(db, title).await;
        let copy_id = insert_copy(db, &book_id).await;
        insert_loan(db, &copy_id).await;
        insert_sale(db, &copy_id).await;
        attach_author(db, &book_id, "author-1").await;
        attach_tag(db, &book_id, "tag-1").await;
        attach_collection(db, "collection-1", &book_id).await;
        attach_note(db, &book_id).await;
        (book_id, copy_id)
    }

    async fn count<E: EntityTrait>(db: &DatabaseConnection) -> usize {
        E::find().all(db).await.unwrap().len()
    }

    #[tokio::test]
    async fn delete_book_cascade_removes_book_and_all_dependents() {
        let db = setup_db().await;
        let (book_id, _copy) = seed_full_book(&db, "doomed").await;

        delete_book_cascade(&db, &book_id).await.unwrap();

        assert_eq!(count::<book::Entity>(&db).await, 0, "book");
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
    async fn delete_book_cascade_leaves_sibling_book_untouched() {
        let db = setup_db().await;
        let (doomed, _) = seed_full_book(&db, "doomed").await;
        let (survivor, _) = seed_full_book(&db, "survivor").await;

        delete_book_cascade(&db, &doomed).await.unwrap();

        // The survivor keeps exactly its own single row in every table.
        assert!(
            book::Entity::find_by_id(survivor.clone())
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(count::<book::Entity>(&db).await, 1, "book");
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
    async fn delete_book_cascade_on_unknown_book_is_noop() {
        let db = setup_db().await;
        let (kept, _) = seed_full_book(&db, "kept").await;

        delete_book_cascade(&db, "does-not-exist").await.unwrap();

        assert!(
            book::Entity::find_by_id(kept)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "unrelated book must survive a no-op delete"
        );
        assert_eq!(count::<copy::Entity>(&db).await, 1);
    }

    #[tokio::test]
    async fn delete_collection_links_drops_links_but_keeps_books() {
        let db = setup_db().await;
        let kept = insert_book(&db, "kept").await;
        let other = insert_book(&db, "other").await;
        attach_collection(&db, "collection-1", &kept).await;
        attach_collection(&db, "collection-1", &other).await;
        attach_collection(&db, "collection-2", &other).await;

        delete_collection_links(&db, "collection-1").await.unwrap();

        // Only collection-2's link survives; both books remain.
        assert_eq!(count::<collection_book::Entity>(&db).await, 1);
        assert_eq!(count::<book::Entity>(&db).await, 2);
    }
}
