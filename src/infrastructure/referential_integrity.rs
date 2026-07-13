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

use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, QueryTrait,
};

use crate::models::{
    author, book, book_authors, book_tags, collection, collection_book, copy, loan, sale, tag,
};
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
    // Copies and their loans/sales first (see `delete_copies_of_book_cascade`).
    delete_copies_of_book_cascade(conn, book_uuid).await?;

    // Remaining direct children of the book.
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

/// Delete every copy of a book together with the loans and sales that
/// referenced those copies, leaving the book row itself in place. Used when a
/// book loses all its physical copies (e.g. ownership turned off) without being
/// removed. Mirrors the `copy -> {loans, sales}` cascade dropped by the
/// UUID-PK rebuild (ADR-044).
///
/// Loans and sales are matched with a subquery on the book's copies so each
/// delete is a single statement (an empty book naturally deletes nothing), then
/// the copies go. Pass a transaction when atomicity matters.
pub async fn delete_copies_of_book_cascade<C>(conn: &C, book_uuid: &str) -> Result<(), DbErr>
where
    C: ConnectionTrait,
{
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
    copy::Entity::delete_many()
        .filter(copy::Column::BookId.eq(book_uuid))
        .exec(conn)
        .await?;

    Ok(())
}

/// Delete a single copy together with the loans and sales that referenced it,
/// in the caller-provided connection. Mirrors the `copy -> {loans, sales}`
/// cascade dropped by the UUID-PK rebuild (ADR-044).
///
/// Returns `true` if a copy row was actually removed, so callers can keep
/// their not-found semantics (and roll back if the copy did not exist). Pass a
/// transaction so the child deletes and the copy delete commit together.
pub async fn delete_copy_cascade<C>(conn: &C, copy_uuid: &str) -> Result<bool, DbErr>
where
    C: ConnectionTrait,
{
    loan::Entity::delete_many()
        .filter(loan::Column::CopyId.eq(copy_uuid))
        .exec(conn)
        .await?;
    sale::Entity::delete_many()
        .filter(sale::Column::CopyId.eq(copy_uuid))
        .exec(conn)
        .await?;

    let result = copy::Entity::delete_by_id(copy_uuid.to_owned())
        .exec(conn)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Delete an author together with the book-author links that referenced it, in
/// the caller-provided connection. Mirrors the `author -> book_authors` cascade
/// dropped by the UUID-PK rebuild (ADR-044).
///
/// Returns `true` if an author row was actually removed, so callers can keep
/// their not-found semantics. Pass a transaction.
pub async fn delete_author_cascade<C>(conn: &C, author_uuid: &str) -> Result<bool, DbErr>
where
    C: ConnectionTrait,
{
    book_authors::Entity::delete_many()
        .filter(book_authors::Column::AuthorId.eq(author_uuid))
        .exec(conn)
        .await?;

    let result = author::Entity::delete_by_id(author_uuid.to_owned())
        .exec(conn)
        .await?;
    Ok(result.rows_affected > 0)
}

/// Delete a tag together with the book-tag links that referenced it, and clear
/// the `parent_id` of its child tags. Mirrors the `tag -> book_tags` cascade
/// and the self-referential `tags.parent_id` ON DELETE SET NULL, both dropped
/// by the UUID-PK rebuild (ADR-044).
///
/// Returns `true` if a tag row was actually removed, so callers can keep their
/// not-found semantics. Pass a transaction.
pub async fn delete_tag_cascade<C>(conn: &C, tag_uuid: &str) -> Result<bool, DbErr>
where
    C: ConnectionTrait,
{
    // Re-parent the children to root: the parent link used to be SET NULL when
    // the parent tag was deleted, so a deleted tag must not leave its children
    // pointing at a vanished parent.
    tag::Entity::update_many()
        .col_expr(tag::Column::ParentId, Expr::value(Option::<String>::None))
        .filter(tag::Column::ParentId.eq(tag_uuid))
        .exec(conn)
        .await?;
    book_tags::Entity::delete_many()
        .filter(book_tags::Column::TagId.eq(tag_uuid))
        .exec(conn)
        .await?;

    let result = tag::Entity::delete_by_id(tag_uuid.to_owned())
        .exec(conn)
        .await?;
    Ok(result.rows_affected > 0)
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

/// Repair referential integrity after a merge applied an inbound change for one
/// entity. If the entity is a replicated PARENT and its row is now ABSENT (the
/// merge deleted it), remove the orphan children it left behind by running the
/// matching cascade. A still-present row (the change was an insert or update) or
/// an unknown entity type is a no-op; the returned bool says whether a cascade
/// ran.
///
/// This is the safe counterpart to a blanket "delete children whose parent is
/// missing" sweep: it acts ONLY when a real delete was merged in (the row is
/// gone), never on a parent that is merely not-yet-synced. A blanket sweep
/// cannot tell those apart and would delete - and propagate the deletion of - a
/// legitimately in-flight row during a cold or partial sync. Accepts the
/// singular entity name and the table name for each parent so it matches either
/// naming the merge layer uses. Pass a transaction.
pub async fn cascade_inbound_delete<C>(
    conn: &C,
    entity_type: &str,
    entity_uuid: &str,
) -> Result<bool, DbErr>
where
    C: ConnectionTrait,
{
    // Map the entity name (singular or table form) to a known replicated parent.
    enum Parent {
        Book,
        Copy,
        Author,
        Tag,
        Collection,
    }
    let parent = match entity_type {
        "book" | "books" => Parent::Book,
        "copy" | "copies" => Parent::Copy,
        "author" | "authors" => Parent::Author,
        "tag" | "tags" => Parent::Tag,
        "collection" | "collections" => Parent::Collection,
        _ => return Ok(false),
    };

    // Act only when the parent row is now absent: a present row means the change
    // was an insert or update, not the delete we repair after. (Match guards
    // cannot host the fallible async lookup, hence the explicit two phases.)
    let id = entity_uuid.to_owned();
    let absent = match parent {
        Parent::Book => book::Entity::find_by_id(id).one(conn).await?.is_none(),
        Parent::Copy => copy::Entity::find_by_id(id).one(conn).await?.is_none(),
        Parent::Author => author::Entity::find_by_id(id).one(conn).await?.is_none(),
        Parent::Tag => tag::Entity::find_by_id(id).one(conn).await?.is_none(),
        Parent::Collection => collection::Entity::find_by_id(id)
            .one(conn)
            .await?
            .is_none(),
    };
    if !absent {
        return Ok(false);
    }

    match parent {
        Parent::Book => delete_book_cascade(conn, entity_uuid).await?,
        Parent::Copy => {
            delete_copy_cascade(conn, entity_uuid).await?;
        }
        Parent::Author => {
            delete_author_cascade(conn, entity_uuid).await?;
        }
        Parent::Tag => {
            delete_tag_cascade(conn, entity_uuid).await?;
        }
        Parent::Collection => delete_collection_links(conn, entity_uuid).await?,
    }
    Ok(true)
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

    async fn insert_author(db: &DatabaseConnection, id: &str, name: &str) {
        author::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(name.to_owned()),
            created_at: Set(now()),
            updated_at: Set(now()),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn insert_tag(db: &DatabaseConnection, id: &str, name: &str, parent_id: Option<&str>) {
        tag::ActiveModel {
            id: Set(id.to_owned()),
            name: Set(name.to_owned()),
            parent_id: Set(parent_id.map(|p| p.to_owned())),
            path: Set(name.to_owned()),
            created_at: Set(now()),
            updated_at: Set(now()),
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
            volume_number: Set(None),
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

    #[tokio::test]
    async fn delete_copy_cascade_removes_copy_and_its_loans_and_sales() {
        let db = setup_db().await;
        let book_id = insert_book(&db, "book").await;
        let doomed = insert_copy(&db, &book_id).await;
        insert_loan(&db, &doomed).await;
        insert_sale(&db, &doomed).await;
        // A sibling copy of the same book, with its own loan and sale.
        let survivor = insert_copy(&db, &book_id).await;
        insert_loan(&db, &survivor).await;
        insert_sale(&db, &survivor).await;

        let existed = delete_copy_cascade(&db, &doomed).await.unwrap();

        assert!(existed, "an existing copy must report as removed");
        assert_eq!(count::<copy::Entity>(&db).await, 1, "only the sibling copy");
        assert_eq!(count::<loan::Entity>(&db).await, 1, "sibling loan survives");
        assert_eq!(count::<sale::Entity>(&db).await, 1, "sibling sale survives");
        assert!(
            copy::Entity::find_by_id(survivor)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn delete_copy_cascade_on_unknown_copy_returns_false() {
        let db = setup_db().await;
        let book_id = insert_book(&db, "book").await;
        let kept = insert_copy(&db, &book_id).await;
        insert_loan(&db, &kept).await;

        let existed = delete_copy_cascade(&db, "does-not-exist").await.unwrap();

        assert!(!existed, "an unknown copy must report as not removed");
        assert_eq!(count::<copy::Entity>(&db).await, 1);
        assert_eq!(count::<loan::Entity>(&db).await, 1);
    }

    #[tokio::test]
    async fn delete_copies_of_book_cascade_clears_copies_but_keeps_the_book() {
        let db = setup_db().await;
        let book_id = insert_book(&db, "unowned").await;
        let c1 = insert_copy(&db, &book_id).await;
        let c2 = insert_copy(&db, &book_id).await;
        insert_loan(&db, &c1).await;
        insert_sale(&db, &c2).await;
        // A second book with its own copy/loan/sale must be left untouched.
        let other_book = insert_book(&db, "owned").await;
        let oc = insert_copy(&db, &other_book).await;
        insert_loan(&db, &oc).await;
        insert_sale(&db, &oc).await;

        delete_copies_of_book_cascade(&db, &book_id).await.unwrap();

        // The book row stays; only its copies and their loans/sales are gone.
        assert!(
            book::Entity::find_by_id(book_id)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "the book itself must remain"
        );
        assert_eq!(count::<copy::Entity>(&db).await, 1, "only the other copy");
        assert_eq!(count::<loan::Entity>(&db).await, 1, "only the other loan");
        assert_eq!(count::<sale::Entity>(&db).await, 1, "only the other sale");
    }

    #[tokio::test]
    async fn delete_author_cascade_removes_author_and_its_book_links() {
        let db = setup_db().await;
        let book_id = insert_book(&db, "book").await;
        insert_author(&db, "author-doomed", "Doomed").await;
        insert_author(&db, "author-kept", "Kept").await;
        attach_author(&db, &book_id, "author-doomed").await;
        attach_author(&db, &book_id, "author-kept").await;

        let existed = delete_author_cascade(&db, "author-doomed").await.unwrap();

        assert!(existed, "an existing author must report as removed");
        assert_eq!(
            count::<author::Entity>(&db).await,
            1,
            "only the kept author"
        );
        assert_eq!(
            count::<book_authors::Entity>(&db).await,
            1,
            "only the kept author's link"
        );
    }

    #[tokio::test]
    async fn delete_author_cascade_on_unknown_author_returns_false() {
        let db = setup_db().await;
        insert_author(&db, "author-kept", "Kept").await;

        let existed = delete_author_cascade(&db, "does-not-exist").await.unwrap();

        assert!(!existed, "an unknown author must report as not removed");
        assert_eq!(count::<author::Entity>(&db).await, 1);
    }

    #[tokio::test]
    async fn delete_tag_cascade_removes_links_and_reparents_children() {
        let db = setup_db().await;
        let book_id = insert_book(&db, "book").await;
        insert_tag(&db, "parent", "Parent", None).await;
        insert_tag(&db, "child-a", "Child A", Some("parent")).await;
        insert_tag(&db, "child-b", "Child B", Some("parent")).await;
        attach_tag(&db, &book_id, "parent").await;
        // A sibling tag with its own child and book link, untouched by the delete.
        insert_tag(&db, "other", "Other", None).await;
        insert_tag(&db, "other-child", "Other Child", Some("other")).await;
        attach_tag(&db, &book_id, "other").await;

        let existed = delete_tag_cascade(&db, "parent").await.unwrap();

        assert!(existed, "an existing tag must report as removed");
        // The tag is gone; its children survive but are re-parented to root.
        assert!(
            tag::Entity::find_by_id("parent".to_owned())
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        for child in ["child-a", "child-b"] {
            let row = tag::Entity::find_by_id(child.to_owned())
                .one(&db)
                .await
                .unwrap()
                .expect("child tag must survive");
            assert_eq!(row.parent_id, None, "child must be re-parented to root");
        }
        // Only the deleted tag's book link is removed.
        assert_eq!(
            count::<book_tags::Entity>(&db).await,
            1,
            "only other's link"
        );
        // The sibling subtree is intact.
        let other_child = tag::Entity::find_by_id("other-child".to_owned())
            .one(&db)
            .await
            .unwrap()
            .expect("sibling child must survive");
        assert_eq!(other_child.parent_id.as_deref(), Some("other"));
        assert_eq!(
            count::<tag::Entity>(&db).await,
            4,
            "parent gone, four remain"
        );
    }

    #[tokio::test]
    async fn delete_tag_cascade_on_unknown_tag_returns_false() {
        let db = setup_db().await;
        insert_tag(&db, "kept", "Kept", None).await;

        let existed = delete_tag_cascade(&db, "does-not-exist").await.unwrap();

        assert!(!existed, "an unknown tag must report as not removed");
        assert_eq!(count::<tag::Entity>(&db).await, 1);
    }

    #[tokio::test]
    async fn cascade_inbound_delete_cleans_orphans_of_a_merged_away_book() {
        let db = setup_db().await;
        let (book_id, _copy) = seed_full_book(&db, "merged-away").await;
        // Simulate the merge having removed just the book row (a delete arrived
        // from another device), leaving this device's children orphaned.
        book::Entity::delete_by_id(book_id.clone())
            .exec(&db)
            .await
            .unwrap();

        let ran = cascade_inbound_delete(&db, "book", &book_id).await.unwrap();

        assert!(ran, "an absent parent must trigger the cascade");
        assert_eq!(count::<copy::Entity>(&db).await, 0, "orphan copies removed");
        assert_eq!(count::<loan::Entity>(&db).await, 0, "orphan loans removed");
        assert_eq!(count::<sale::Entity>(&db).await, 0, "orphan sales removed");
        assert_eq!(count::<book_authors::Entity>(&db).await, 0);
        assert_eq!(count::<book_tags::Entity>(&db).await, 0);
        assert_eq!(count::<collection_book::Entity>(&db).await, 0);
        assert_eq!(count::<book_note::Entity>(&db).await, 0);
    }

    #[tokio::test]
    async fn cascade_inbound_delete_is_noop_when_parent_still_present() {
        let db = setup_db().await;
        let (book_id, _copy) = seed_full_book(&db, "still-here").await;

        // Plural table name, present parent (the change was an insert/update).
        let ran = cascade_inbound_delete(&db, "books", &book_id)
            .await
            .unwrap();

        assert!(!ran, "a present parent must not trigger the cascade");
        assert_eq!(count::<copy::Entity>(&db).await, 1, "children untouched");
        assert_eq!(count::<book::Entity>(&db).await, 1);
    }

    #[tokio::test]
    async fn cascade_inbound_delete_ignores_unknown_entity_type() {
        let db = setup_db().await;
        let ran = cascade_inbound_delete(&db, "widget", "whatever")
            .await
            .unwrap();
        assert!(!ran, "an unknown entity type must be a no-op");
    }
}
