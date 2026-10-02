//! Household readers: managing who reads on this device.
//!
//! The rules live in `domain::household`; this is the door the FFI surface
//! goes through, and where a use case made of several writes gets its
//! transaction.

use sea_orm::{ConnectionTrait, TransactionTrait};

use crate::domain::{DomainError, HouseholdRepository, Reader};
use crate::infrastructure::repositories::SeaOrmHouseholdRepository;

/// Every reader of the household, oldest first. Empty until someone opts in.
pub async fn list_readers<C: ConnectionTrait>(db: &C) -> Result<Vec<Reader>, DomainError> {
    SeaOrmHouseholdRepository::new(db).list_readers().await
}

/// Who reads on this device. `None`: the device shows the shared book columns.
pub async fn current_reader<C: ConnectionTrait>(db: &C) -> Result<Option<Reader>, DomainError> {
    SeaOrmHouseholdRepository::new(db).current_reader().await
}

/// Add a reader to the household and make it the reader of this device (see
/// `HouseholdRepository::create_reader` for the first-reader rule).
pub async fn create_reader<C>(db: &C, name: &str) -> Result<Reader, DomainError>
where
    C: ConnectionTrait + TransactionTrait,
{
    let txn = db.begin().await?;
    let reader = SeaOrmHouseholdRepository::new(&txn)
        .create_reader(name)
        .await?;
    txn.commit().await?;
    Ok(reader)
}

/// Make an existing reader the reader of this device.
pub async fn set_current_reader<C: ConnectionTrait>(
    db: &C,
    reader_id: &str,
) -> Result<(), DomainError> {
    SeaOrmHouseholdRepository::new(db)
        .set_current_reader(reader_id)
        .await
}

/// Put this device back on the shared reading state. Readers and their
/// readings are kept.
pub async fn clear_current_reader<C: ConnectionTrait>(db: &C) -> Result<(), DomainError> {
    SeaOrmHouseholdRepository::new(db)
        .clear_current_reader()
        .await
}

pub async fn rename_reader<C: ConnectionTrait>(
    db: &C,
    reader_id: &str,
    name: &str,
) -> Result<(), DomainError> {
    SeaOrmHouseholdRepository::new(db)
        .rename_reader(reader_id, name)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MAX_READER_NAME_CHARS, ReadingChange};
    use crate::models::Book;
    use sea_orm::{DatabaseConnection, Statement};

    async fn migrated_db() -> DatabaseConnection {
        crate::db::init_db("sqlite::memory:")
            .await
            .expect("init db")
    }

    async fn insert_book(db: &DatabaseConnection, title: &str, status: &str) -> String {
        let created = crate::services::book_service::create_book(
            db,
            Book {
                title: title.to_owned(),
                reading_status: Some(status.to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("create book");
        created.id.expect("id")
    }

    async fn status_seen(db: &DatabaseConnection, id: &str) -> String {
        crate::services::book_service::get_book(db, id)
            .await
            .expect("get book")
            .reading_status
            .expect("status")
    }

    #[tokio::test]
    async fn without_a_reader_nothing_changes() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "read").await;

        assert!(current_reader(&db).await.unwrap().is_none());
        assert_eq!(status_seen(&db, &id).await, "read");
        SeaOrmHouseholdRepository::new(&db)
            .record(&id, ReadingChange::default())
            .await
            .unwrap();
        let n = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT count(*) AS n FROM book_readings".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "n")
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn first_reader_inherits_the_history_and_later_readers_start_blank() {
        let db = migrated_db().await;
        let read = insert_book(&db, "Dune", "read").await;
        let wished = insert_book(&db, "Hyperion", "wanting").await;

        let owner = create_reader(&db, "Bruno").await.unwrap();
        assert_eq!(status_seen(&db, &read).await, "read");
        assert_eq!(status_seen(&db, &wished).await, "wanting");

        let partner = create_reader(&db, "Alice").await.unwrap();
        assert_eq!(
            SeaOrmHouseholdRepository::new(&db)
                .current_reader_id()
                .await
                .unwrap(),
            Some(partner.id)
        );
        assert_eq!(status_seen(&db, &read).await, "");
        // The wishlist is the household's.
        assert_eq!(status_seen(&db, &wished).await, "wanting");

        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(status_seen(&db, &read).await, "read");
    }

    #[tokio::test]
    async fn each_reader_keeps_their_own_status_dates_and_rating() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "to_read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();

        // Alice (current) finishes it and rates it.
        let mut book = crate::services::book_service::get_book(&db, &id)
            .await
            .unwrap();
        book.reading_status = Some("read".to_owned());
        book.finished_reading_at = Some(Some("2026-09-01".to_owned()));
        book.user_rating = Some(8);
        let returned = crate::services::book_service::update_book(&db, &id, book)
            .await
            .unwrap();
        assert_eq!(returned.reading_status.as_deref(), Some("read"));

        set_current_reader(&db, &owner.id).await.unwrap();
        let mine = crate::services::book_service::get_book(&db, &id)
            .await
            .unwrap();
        assert_eq!(mine.reading_status.as_deref(), Some("to_read"));
        assert_eq!(mine.finished_reading_at, Some(None));
        assert_eq!(mine.user_rating, None);

        set_current_reader(&db, &partner.id).await.unwrap();
        let hers = crate::services::book_service::get_book(&db, &id)
            .await
            .unwrap();
        assert_eq!(hers.reading_status.as_deref(), Some("read"));
        assert_eq!(
            hers.finished_reading_at,
            Some(Some("2026-09-01".to_owned()))
        );
        assert_eq!(hers.user_rating, Some(8));
    }

    async fn set_status(db: &DatabaseConnection, id: &str, status: &str) {
        let mut book = crate::services::book_service::get_book(db, id)
            .await
            .unwrap();
        book.reading_status = Some(status.to_owned());
        crate::services::book_service::update_book(db, id, book)
            .await
            .unwrap();
    }

    async fn stored_status(db: &DatabaseConnection, id: &str) -> String {
        db.query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT reading_status FROM books WHERE uuid = ?",
            [id.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "reading_status")
        .unwrap()
    }

    #[tokio::test]
    async fn the_wishlist_is_shared_and_leaving_it_is_too() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();

        // Alice wishes for it: Bruno, who has no reading of his own for
        // it, sees it wished too.
        set_status(&db, &id, "wanting").await;
        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "wanting");

        // Bruno, looking at a wished book, starts reading it: it leaves
        // the wishlist for both, and Alice keeps her own status (none).
        set_status(&db, &id, "reading").await;
        assert_eq!(status_seen(&db, &id).await, "reading");
        assert_eq!(stored_status(&db, &id).await, "reading");
        set_current_reader(&db, &partner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "");
    }

    /// A book read without being owned (borrowed, say): the only kind a
    /// household can sensibly wish for.
    async fn insert_unowned_book(db: &DatabaseConnection, title: &str, status: &str) -> String {
        let created = crate::services::book_service::create_book(
            db,
            Book {
                title: title.to_owned(),
                reading_status: Some(status.to_owned()),
                owned: Some(false),
                ..Default::default()
            },
        )
        .await
        .expect("create book");
        created.id.expect("id")
    }

    #[tokio::test]
    async fn acquiring_a_wished_book_ends_the_wish_whoever_does_it() {
        let db = migrated_db().await;
        let id = insert_unowned_book(&db, "Dune", "read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();
        set_status(&db, &id, "wanting").await;

        // Bruno, who has his own reading of it, records that the household
        // now owns it: an owned book must not stay on the wishlist.
        set_current_reader(&db, &owner.id).await.unwrap();
        let mut book = crate::services::book_service::get_book(&db, &id)
            .await
            .unwrap();
        assert_eq!(book.reading_status.as_deref(), Some("read"));
        book.owned = Some(true);
        crate::services::book_service::update_book(&db, &id, book)
            .await
            .unwrap();
        assert_eq!(stored_status(&db, &id).await, "read");
        assert_eq!(status_seen(&db, &id).await, "read");
        set_current_reader(&db, &partner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "");
    }

    #[tokio::test]
    async fn recording_a_reading_leaves_the_wish_of_a_reader_with_a_status() {
        let db = migrated_db().await;
        let id = insert_unowned_book(&db, "Dune", "reading").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();
        set_status(&db, &id, "wanting").await;

        set_current_reader(&db, &owner.id).await.unwrap();
        let record = crate::services::book_service::record_read_book(
            &db,
            Book {
                title: "Dune".to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!record.created);
        assert_eq!(record.book.reading_status.as_deref(), Some("read"));
        assert_eq!(stored_status(&db, &id).await, "wanting");
        set_current_reader(&db, &partner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "wanting");
    }

    #[tokio::test]
    async fn a_wish_does_not_mask_another_readers_reading() {
        let db = migrated_db().await;
        let id = insert_unowned_book(&db, "Dune", "read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();

        // Alice (current, no reading of her own) wishes for it.
        set_status(&db, &id, "wanting").await;
        assert_eq!(status_seen(&db, &id).await, "wanting");

        // Bruno read it: his reading shows, the wish does not replace it.
        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "read");

        // Bruno moving his own reading leaves the household's wish alone.
        set_status(&db, &id, "reading").await;
        assert_eq!(status_seen(&db, &id).await, "reading");
        assert_eq!(stored_status(&db, &id).await, "wanting");
        set_current_reader(&db, &partner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "wanting");
    }

    #[tokio::test]
    async fn wishing_replaces_the_readers_own_status() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "to_read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();

        // The status picker holds one value: choosing the wish gives up the
        // reader's own status, or the choice would not show.
        set_status(&db, &id, "wanting").await;
        assert_eq!(status_seen(&db, &id).await, "wanting");
        assert_eq!(stored_status(&db, &id).await, "wanting");

        let wished = || crate::services::book_service::BookFilter {
            status: Some("wanting".to_owned()),
            ..Default::default()
        };
        let list = crate::services::book_service::list_books(&db, wished())
            .await
            .unwrap();
        assert_eq!(list.len(), 1);

        // A reader with a reading of their own finds the book under that
        // reading AND on the wishlist, which is the household's: the book
        // carries their status plus the wish flag.
        let partner = create_reader(&db, "Alice").await.unwrap();
        set_current_reader(&db, &owner.id).await.unwrap();
        set_status(&db, &id, "reading").await;
        set_current_reader(&db, &partner.id).await.unwrap();
        set_status(&db, &id, "wanting").await;
        set_current_reader(&db, &owner.id).await.unwrap();
        let reading = crate::services::book_service::list_books(
            &db,
            crate::services::book_service::BookFilter {
                status: Some("reading".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(reading.len(), 1);
        assert_eq!(reading[0].wanted, Some(true));
        let list = crate::services::book_service::list_books(&db, wished())
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reading_status.as_deref(), Some("reading"));
        assert_eq!(list[0].wanted, Some(true));
    }

    #[tokio::test]
    async fn peers_still_see_the_wish_from_a_reader_with_a_status() {
        let db = migrated_db().await;
        let id = insert_unowned_book(&db, "Dune", "read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        create_reader(&db, "Alice").await.unwrap();
        set_status(&db, &id, "wanting").await;

        // On Bruno's device the book reads "read": the wish must still reach
        // the peers, who learn it from the flag once the status is stripped.
        set_current_reader(&db, &owner.id).await.unwrap();
        let mut book = crate::services::book_service::get_book(&db, &id)
            .await
            .unwrap();
        assert_eq!(book.reading_status.as_deref(), Some("read"));
        assert!(book.is_wished());
        book.redact_for_peer();
        assert_eq!(book.reading_status, None);
        assert_eq!(book.wanted, Some(true));
    }

    /// The HTTP update handler fires the "a wish is available" scan on the
    /// transition INTO the wish, judged on the books it gets from the
    /// repository before and after. Those carry the reader view, so the
    /// judgement must rest on the wish itself (`Book::is_wished`), not on the
    /// status shown.
    #[tokio::test]
    async fn the_wish_transition_is_judged_on_the_wish_itself() {
        use crate::domain::BookRepository;
        use crate::infrastructure::repositories::SeaOrmBookRepository;

        let db = migrated_db().await;
        let repo = SeaOrmBookRepository::new(db.clone());
        let id = insert_unowned_book(&db, "Dune", "read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        create_reader(&db, "Alice").await.unwrap();
        set_status(&db, &id, "wanting").await;

        // Bruno, shown "read", picks the wish on a book already wished for:
        // no transition, the scan must not run again.
        set_current_reader(&db, &owner.id).await.unwrap();
        let current = repo.find_by_id(&id).await.unwrap().unwrap();
        assert_eq!(current.reading_status.as_deref(), Some("read"));
        let mut change = current.clone();
        change.reading_status = Some("wanting".to_owned());
        let updated = repo.update(&id, change).await.unwrap();
        assert!(!(updated.is_wished() && !current.is_wished()));

        // A first wish on another book: the transition is seen.
        let fresh = insert_unowned_book(&db, "Hyperion", "to_read").await;
        let current = repo.find_by_id(&fresh).await.unwrap().unwrap();
        let mut change = current.clone();
        change.reading_status = Some("wanting".to_owned());
        let updated = repo.update(&fresh, change).await.unwrap();
        assert!(updated.is_wished() && !current.is_wished());
    }

    #[tokio::test]
    async fn a_reader_with_a_status_can_take_a_book_off_the_wishlist() {
        let db = migrated_db().await;
        let id = insert_unowned_book(&db, "Dune", "read").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();
        set_status(&db, &id, "wanting").await;

        set_current_reader(&db, &owner.id).await.unwrap();
        let book = crate::services::book_service::remove_wish(&db, &id)
            .await
            .unwrap();
        // Bruno keeps his reading, the wish is gone for both.
        assert_eq!(book.reading_status.as_deref(), Some("read"));
        assert!(!book.is_wished());
        assert_eq!(stored_status(&db, &id).await, "");
        set_current_reader(&db, &partner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "");

        // Nothing to remove: the book is left alone.
        let read = insert_book(&db, "Hyperion", "read").await;
        crate::services::book_service::remove_wish(&db, &read)
            .await
            .unwrap();
        assert_eq!(stored_status(&db, &read).await, "read");
    }

    #[tokio::test]
    async fn the_status_filter_follows_the_current_reader() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "read").await;
        create_reader(&db, "Bruno").await.unwrap();
        create_reader(&db, "Alice").await.unwrap();

        let read_filter = || crate::services::book_service::BookFilter {
            status: Some("read".to_owned()),
            ..Default::default()
        };
        let read = crate::services::book_service::list_books(&db, read_filter())
            .await
            .unwrap();
        assert!(read.is_empty(), "Alice has not read it");

        let owner = list_readers(&db).await.unwrap().remove(0);
        set_current_reader(&db, &owner.id).await.unwrap();
        let read = crate::services::book_service::list_books(&db, read_filter())
            .await
            .unwrap();
        assert!(read.iter().any(|b| b.id.as_deref() == Some(id.as_str())));
    }

    #[tokio::test]
    async fn deleting_a_book_deletes_every_readers_state() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "read").await;
        let reader = create_reader(&db, "Bruno").await.unwrap();

        crate::services::book_service::delete_book(&db, &id)
            .await
            .unwrap();
        let readings = SeaOrmHouseholdRepository::new(&db)
            .readings_of(&reader.id, None)
            .await
            .unwrap();
        assert!(readings.is_empty());
    }

    /// Bruno (first reader) has read "Dune" in 2026; Alice has not. Leaves
    /// Alice as the reader of the device.
    async fn one_reader_has_read_dune(db: &DatabaseConnection) -> (String, Reader, Reader) {
        let id = insert_book(db, "Dune", "to_read").await;
        let owner = create_reader(db, "Bruno").await.unwrap();
        let mut book = crate::services::book_service::get_book(db, &id)
            .await
            .unwrap();
        book.reading_status = Some("read".to_owned());
        book.finished_reading_at = Some(Some("2026-09-01".to_owned()));
        book.user_rating = Some(9);
        crate::services::book_service::update_book(db, &id, book)
            .await
            .unwrap();
        let partner = create_reader(db, "Alice").await.unwrap();
        (id, owner, partner)
    }

    #[tokio::test]
    async fn the_reading_counters_follow_the_current_reader() {
        use crate::domain::GamificationRepository;
        use crate::infrastructure::repositories::SeaOrmGamificationRepository;

        let db = migrated_db().await;
        let (_, owner, _) = one_reader_has_read_dune(&db).await;
        let repo = SeaOrmGamificationRepository::new(db.clone());

        assert_eq!(repo.count_books_read().await.unwrap(), 0);
        assert_eq!(repo.count_books_read_in_year("2026").await.unwrap(), 0);
        // The library itself is shared.
        assert_eq!(repo.count_books().await.unwrap(), 1);

        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(repo.count_books_read().await.unwrap(), 1);
        assert_eq!(repo.count_books_read_in_year("2026").await.unwrap(), 1);
        assert_eq!(repo.count_books_read_in_year("2025").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_collection_shows_the_current_readers_status() {
        use crate::domain::{CollectionRepository, CreateCollectionInput};
        use crate::infrastructure::repositories::SeaOrmCollectionRepository;

        let db = migrated_db().await;
        let (id, owner, _) = one_reader_has_read_dune(&db).await;
        let repo = SeaOrmCollectionRepository::new(db.clone());
        let collection = repo
            .create(CreateCollectionInput {
                name: "Cycle".to_owned(),
                description: None,
                source: None,
            })
            .await
            .unwrap()
            .id;
        repo.add_book(&collection, &id).await.unwrap();

        let books = repo.get_books(&collection).await.unwrap();
        assert_eq!(books[0].reading_status.as_deref(), Some(""));

        set_current_reader(&db, &owner.id).await.unwrap();
        let books = repo.get_books(&collection).await.unwrap();
        assert_eq!(books[0].reading_status.as_deref(), Some("read"));
    }

    #[tokio::test]
    async fn the_recommendations_score_the_current_readers_readings() {
        let db = migrated_db().await;
        let (_, owner, _) = one_reader_has_read_dune(&db).await;

        let rows = crate::services::recommendation_service::load_scoring_books(&db)
            .await
            .unwrap();
        assert_eq!(rows[0].raw_status, "");
        assert_eq!(rows[0].book.user_rating, None);

        set_current_reader(&db, &owner.id).await.unwrap();
        let rows = crate::services::recommendation_service::load_scoring_books(&db)
            .await
            .unwrap();
        assert_eq!(rows[0].raw_status, "read");
        assert_eq!(rows[0].book.user_rating, Some(9));
    }

    #[tokio::test]
    async fn the_repository_write_path_records_the_current_readers_reading() {
        use crate::domain::BookRepository;
        use crate::infrastructure::repositories::SeaOrmBookRepository;

        let db = migrated_db().await;
        let id = insert_unowned_book(&db, "Dune", "reading").await;
        let owner = create_reader(&db, "Bruno").await.unwrap();
        let partner = create_reader(&db, "Alice").await.unwrap();
        set_status(&db, &id, "wanting").await;
        let repo = SeaOrmBookRepository::new(db.clone());

        // Bruno finishes it through the repository (the HTTP write path):
        // his reading is recorded, the returned book shows it, and the wish
        // stays on the book row.
        set_current_reader(&db, &owner.id).await.unwrap();
        let mut book = repo.find_by_id(&id).await.unwrap().unwrap();
        assert_eq!(book.reading_status.as_deref(), Some("reading"));
        book.reading_status = Some("read".to_owned());
        let updated = repo.update(&id, book).await.unwrap();
        assert_eq!(updated.reading_status.as_deref(), Some("read"));
        assert_eq!(status_seen(&db, &id).await, "read");
        assert_eq!(stored_status(&db, &id).await, "wanting");
        set_current_reader(&db, &partner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "wanting");

        // A book created through the repository starts with the creator's
        // reading, and blank for the other reader.
        let created = repo
            .create(Book {
                title: "Hyperion".to_owned(),
                reading_status: Some("read".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();
        let created_id = created.id.unwrap();
        assert_eq!(status_seen(&db, &created_id).await, "read");
        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(status_seen(&db, &created_id).await, "");
    }

    #[tokio::test]
    async fn a_reader_name_is_bounded_on_create_and_rename() {
        let db = migrated_db().await;
        // Counted in characters, not bytes: accented names get the full length.
        let longest = "é".repeat(MAX_READER_NAME_CHARS);
        let too_long = "é".repeat(MAX_READER_NAME_CHARS + 1);

        assert!(create_reader(&db, &too_long).await.is_err());
        assert!(list_readers(&db).await.unwrap().is_empty());

        // Surrounding whitespace is trimmed before the length is judged.
        let reader = create_reader(&db, &format!("  {longest}  ")).await.unwrap();
        assert_eq!(reader.name, longest);

        assert!(rename_reader(&db, &reader.id, &too_long).await.is_err());
        assert_eq!(list_readers(&db).await.unwrap()[0].name, longest);
        rename_reader(&db, &reader.id, "Alice").await.unwrap();
        assert_eq!(list_readers(&db).await.unwrap()[0].name, "Alice");
    }
}
