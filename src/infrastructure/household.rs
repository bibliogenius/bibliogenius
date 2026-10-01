//! Household readers: one shared library, one reading state per person.
//!
//! Two people who share a library enrol their devices into the same account, so
//! account sync replicates the catalogue between them (ADR-044). The reading
//! state, though, is personal: one of them has read a book, the other has not.
//! `books` carries that state in columns, and a CRR replicates every column, so
//! it cannot hold two readers' answers. This module keeps them beside it:
//!
//! - `readers` (CRR): the people of the household, `id` + display `name`.
//! - `book_readings` (CRR): one row per (book, reader) holding the reading
//!   status, the reading dates and the rating.
//! - `reader_local` (NOT a CRR): who reads on THIS device. A single row.
//!
//! **Opt-in, and a no-op until chosen.** A device with no current reader behaves
//! exactly as before: every function here returns `None` or does nothing, and
//! the `books` columns stay the source of truth. Choosing a reader switches the
//! device to the per-reader state.
//!
//! **The `books` columns keep being written.** Every reading write lands on the
//! book row too (last writer wins), so the paths that still read the columns
//! directly (exports, statistics, peer payloads) keep working with a
//! household-wide answer, and a device without a reader keeps a coherent view.
//!
//! **The wishlist stays shared.** `wanting` is a household decision, not a
//! personal one, so it stays on `books.reading_status` alone and is never stored
//! in `book_readings`. The wish never hides a reading: a reader with a status of
//! their own for a wanted book sees that status, and moving it leaves the wish
//! in place. A reader with no status of their own sees `wanting`, and replacing
//! it takes the book off the wishlist for everyone.

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DbErr, Statement, TransactionTrait};

use crate::models::Book;

/// The status shared by the whole household (see the module docs).
const WANTING: &str = "wanting";

/// Longest reader name accepted, in characters. `readers` is replicated, and
/// the hub rejects a sync block over 64 KB: an unbounded name could make the
/// block impossible to push.
pub const MAX_READER_NAME_CHARS: usize = 50;

/// A person of the household.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    pub id: String,
    pub name: String,
}

/// One reader's state for one book.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    pub reading_status: String,
    pub started_reading_at: Option<String>,
    pub finished_reading_at: Option<String>,
    pub user_rating: Option<i32>,
}

/// A change to a reader's state for one book. `None` leaves a field as it is,
/// with the same meaning as the `Option` fields of an update `Book`.
#[derive(Debug, Clone, Default)]
pub struct ReadingChange {
    pub reading_status: Option<String>,
    pub started_reading_at: Option<Option<String>>,
    pub finished_reading_at: Option<Option<String>>,
    pub user_rating: Option<Option<i32>>,
}

impl ReadingChange {
    /// Everything a create or a full update carries about the reading.
    pub fn from_book(book: &Book) -> Self {
        Self {
            reading_status: book.reading_status.clone(),
            started_reading_at: book.started_reading_at.clone(),
            finished_reading_at: book.finished_reading_at.clone(),
            user_rating: Some(book.user_rating),
        }
    }

    fn apply_to(self, reading: &mut Reading) {
        // `wanting` is the household's, and lives on the book row only. The
        // status picker holds one value, so choosing the wish gives up the
        // reader's own status: kept, it would show in place of the wish.
        if let Some(status) = self.reading_status {
            reading.reading_status = if status == WANTING {
                String::new()
            } else {
                status
            };
        }
        if let Some(started) = self.started_reading_at {
            reading.started_reading_at = started;
        }
        if let Some(finished) = self.finished_reading_at {
            reading.finished_reading_at = finished;
        }
        if let Some(rating) = self.user_rating {
            reading.user_rating = rating;
        }
    }
}

/// The current reader's state for a set of books, ready to lay over `Book` DTOs.
#[derive(Debug, Clone)]
pub struct ReaderView {
    pub reader_id: String,
    readings: HashMap<String, Reading>,
}

impl ReaderView {
    /// Replace the book's reading fields with the current reader's. The
    /// reader's own status comes first: the household wanting a book must not
    /// hide that this reader has read it. A book the reader has no status for
    /// reads as no reading intent, unless the household wants it.
    pub fn apply(&self, book: &mut Book) {
        let Some(id) = book.id.as_deref() else {
            return;
        };
        let wanted = book.reading_status.as_deref() == Some(WANTING);
        let reading = self.readings.get(id).cloned().unwrap_or_default();
        book.reading_status = Some(if reading.reading_status.is_empty() && wanted {
            WANTING.to_owned()
        } else {
            reading.reading_status
        });
        book.started_reading_at = Some(reading.started_reading_at);
        book.finished_reading_at = Some(reading.finished_reading_at);
        book.user_rating = reading.user_rating;
    }

    pub fn apply_all(&self, books: &mut [Book]) {
        for book in books {
            self.apply(book);
        }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Who reads on this device, if a reader was chosen.
pub async fn current_reader_id<C: ConnectionTrait>(db: &C) -> Result<Option<String>, DbErr> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT reader_id FROM reader_local WHERE id = 1".to_owned(),
        ))
        .await?;
    row.map(|r| r.try_get::<String>("", "reader_id"))
        .transpose()
}

/// The current reader, resolved against `readers`. A reader deleted on another
/// device resolves to `None`, which puts this device back on the book columns.
pub async fn current_reader<C: ConnectionTrait>(db: &C) -> Result<Option<Reader>, DbErr> {
    let Some(id) = current_reader_id(db).await? else {
        return Ok(None);
    };
    Ok(list_readers(db).await?.into_iter().find(|r| r.id == id))
}

/// Every reader of the household, oldest first.
pub async fn list_readers<C: ConnectionTrait>(db: &C) -> Result<Vec<Reader>, DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT id, name FROM readers ORDER BY created_at, id".to_owned(),
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(Reader {
                id: r.try_get("", "id")?,
                name: r.try_get("", "name")?,
            })
        })
        .collect()
}

/// The name as stored: trimmed, not empty, within [`MAX_READER_NAME_CHARS`].
fn valid_reader_name(name: &str) -> Result<&str, DbErr> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DbErr::Custom("Reader name is required".to_owned()));
    }
    if name.chars().count() > MAX_READER_NAME_CHARS {
        return Err(DbErr::Custom(format!(
            "Reader name is longer than {MAX_READER_NAME_CHARS} characters"
        )));
    }
    Ok(name)
}

/// Add a reader to the household and make it the reader of this device.
///
/// The FIRST reader of a household inherits the reading state already recorded
/// on the books: it is the owner's history, typed before the household existed.
/// Every later reader starts blank. "First" is judged on what this device has
/// synced, so the owner should create their reader before the others enrol.
pub async fn create_reader<C>(db: &C, name: &str) -> Result<Reader, DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    let name = valid_reader_name(name)?;
    let reader = Reader {
        id: crate::utils::uuid_gen::new_uuid_v7(),
        name: name.to_owned(),
    };
    let now = now();

    let txn = db.begin().await?;
    let first = list_readers(&txn).await?.is_empty();
    txn.execute(Statement::from_sql_and_values(
        txn.get_database_backend(),
        "INSERT INTO readers (id, name, created_at) VALUES (?, ?, ?)",
        [
            reader.id.clone().into(),
            reader.name.clone().into(),
            now.clone().into(),
        ],
    ))
    .await?;
    if first {
        txn.execute(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "INSERT INTO book_readings \
             (book_uuid, reader_id, reading_status, started_reading_at, \
              finished_reading_at, user_rating, updated_at) \
             SELECT uuid, ?, \
                    CASE WHEN reading_status = 'wanting' THEN '' ELSE reading_status END, \
                    started_reading_at, finished_reading_at, user_rating, ? \
             FROM books \
             WHERE reading_status NOT IN ('', 'wanting') \
                OR started_reading_at IS NOT NULL \
                OR finished_reading_at IS NOT NULL \
                OR user_rating IS NOT NULL",
            [reader.id.clone().into(), now.into()],
        ))
        .await?;
    }
    set_current_reader_id(&txn, &reader.id).await?;
    txn.commit().await?;
    Ok(reader)
}

/// Make an existing reader the reader of this device.
pub async fn set_current_reader<C: ConnectionTrait>(db: &C, reader_id: &str) -> Result<(), DbErr> {
    if !list_readers(db).await?.iter().any(|r| r.id == reader_id) {
        return Err(DbErr::RecordNotFound(format!("reader {reader_id}")));
    }
    set_current_reader_id(db, reader_id).await
}

async fn set_current_reader_id<C: ConnectionTrait>(db: &C, reader_id: &str) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO reader_local (id, reader_id) VALUES (1, ?) \
         ON CONFLICT(id) DO UPDATE SET reader_id = excluded.reader_id",
        [reader_id.into()],
    ))
    .await?;
    Ok(())
}

/// Put this device back on the shared book columns. The household and its
/// readings are untouched.
pub async fn clear_current_reader<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM reader_local".to_owned(),
    ))
    .await?;
    Ok(())
}

pub async fn rename_reader<C: ConnectionTrait>(
    db: &C,
    reader_id: &str,
    name: &str,
) -> Result<(), DbErr> {
    let name = valid_reader_name(name)?;
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE readers SET name = ? WHERE id = ?",
        [name.into(), reader_id.into()],
    ))
    .await?;
    Ok(())
}

fn reading_from_row(row: &sea_orm::QueryResult) -> Result<(String, Reading), DbErr> {
    Ok((
        row.try_get("", "book_uuid")?,
        Reading {
            reading_status: row.try_get("", "reading_status")?,
            started_reading_at: row.try_get("", "started_reading_at")?,
            finished_reading_at: row.try_get("", "finished_reading_at")?,
            user_rating: row.try_get("", "user_rating")?,
        },
    ))
}

/// The current reader's state for `book_ids`, or for every book when `None`.
/// `None` when this device has no reader: callers leave the books as stored.
pub async fn current_view<C: ConnectionTrait>(
    db: &C,
    book_ids: Option<&[String]>,
) -> Result<Option<ReaderView>, DbErr> {
    let Some(reader_id) = current_reader(db).await?.map(|r| r.id) else {
        return Ok(None);
    };
    let mut sql = "SELECT book_uuid, reading_status, started_reading_at, \
                   finished_reading_at, user_rating \
                   FROM book_readings WHERE reader_id = ?"
        .to_owned();
    let mut values: Vec<sea_orm::Value> = vec![reader_id.clone().into()];
    if let Some(ids) = book_ids {
        if ids.is_empty() {
            return Ok(Some(ReaderView {
                reader_id,
                readings: HashMap::new(),
            }));
        }
        sql.push_str(&format!(
            " AND book_uuid IN ({})",
            vec!["?"; ids.len()].join(", ")
        ));
        values.extend(ids.iter().map(|id| id.clone().into()));
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            db.get_database_backend(),
            &sql,
            values,
        ))
        .await?;
    let readings = rows
        .iter()
        .map(reading_from_row)
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(Some(ReaderView {
        reader_id,
        readings,
    }))
}

/// The SQL condition on `books` matching `status` for `reader_id`, standing in
/// for `reading_status = ?` when the device has a reader. It answers with the
/// status [`ReaderView::apply`] shows: the reader's own first, the household's
/// wish for a book the reader has no status for.
///
/// Columns are qualified with `books.`: the callers join the authors, whose
/// table has a `uuid` column too.
pub fn status_condition(reader_id: &str, status: &str) -> sea_orm::sea_query::SimpleExpr {
    use sea_orm::sea_query::Expr;

    const NO_OWN_STATUS: &str = "books.uuid NOT IN \
         (SELECT book_uuid FROM book_readings \
          WHERE reader_id = ? AND reading_status != '')";

    if status == WANTING {
        return Expr::cust_with_values(
            format!("(books.reading_status = 'wanting' AND {NO_OWN_STATUS})"),
            [reader_id.to_owned()],
        );
    }
    if status.is_empty() {
        return Expr::cust_with_values(
            format!("(books.reading_status != 'wanting' AND {NO_OWN_STATUS})"),
            [reader_id.to_owned()],
        );
    }
    Expr::cust_with_values(
        "books.uuid IN \
         (SELECT book_uuid FROM book_readings \
          WHERE reader_id = ? AND reading_status = ?)",
        [reader_id.to_owned(), status.to_owned()],
    )
}

/// Whether the current reader's status change must leave the household's wish
/// on the book row. True when the device has a reader holding a status of
/// their own for the book: they were not looking at the wish, so moving their
/// reading says nothing about it. A reader with no status of their own was
/// shown the wish, and replacing it takes the book off the wishlist for all.
pub async fn keeps_wish<C: ConnectionTrait>(db: &C, book_uuid: &str) -> Result<bool, DbErr> {
    let Some(reader_id) = current_reader(db).await?.map(|r| r.id) else {
        return Ok(false);
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT 1 FROM book_readings \
             WHERE book_uuid = ? AND reader_id = ? AND reading_status != ''",
            [book_uuid.into(), reader_id.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// Lay the current reader's state over `books`, when this device has one.
///
/// Best-effort: a failure leaves the books as stored, the household-wide answer,
/// rather than failing the read.
pub async fn overlay<C: ConnectionTrait>(db: &C, books: &mut [Book]) {
    let ids: Vec<String> = books.iter().filter_map(|b| b.id.clone()).collect();
    match current_view(db, Some(&ids)).await {
        Ok(Some(view)) => view.apply_all(books),
        Ok(None) => {}
        Err(e) => tracing::warn!("household overlay skipped: {e}"),
    }
}

/// Record `change` as the current reader's state for `book_uuid`. A no-op on a
/// device with no reader.
pub async fn record<C: ConnectionTrait>(
    db: &C,
    book_uuid: &str,
    change: ReadingChange,
) -> Result<(), DbErr> {
    let Some(reader_id) = current_reader(db).await?.map(|r| r.id) else {
        return Ok(());
    };
    let existing = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT book_uuid, reading_status, started_reading_at, \
             finished_reading_at, user_rating \
             FROM book_readings WHERE book_uuid = ? AND reader_id = ?",
            [book_uuid.into(), reader_id.clone().into()],
        ))
        .await?;
    let mut reading = match &existing {
        Some(row) => reading_from_row(row)?.1,
        None => Reading::default(),
    };
    change.apply_to(&mut reading);

    // A plain INSERT or UPDATE rather than an upsert: cr-sqlite captures both
    // through its triggers, and each statement stays obvious to it.
    let sql = if existing.is_some() {
        "UPDATE book_readings SET reading_status = ?, started_reading_at = ?, \
         finished_reading_at = ?, user_rating = ?, updated_at = ? \
         WHERE book_uuid = ? AND reader_id = ?"
    } else {
        "INSERT INTO book_readings (reading_status, started_reading_at, \
         finished_reading_at, user_rating, updated_at, book_uuid, reader_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    };
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        [
            reading.reading_status.into(),
            reading.started_reading_at.into(),
            reading.finished_reading_at.into(),
            reading.user_rating.into(),
            now().into(),
            book_uuid.into(),
            reader_id.into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Remove every reader's state for a book. Part of `delete_book_cascade`.
pub async fn delete_readings_of_book<C: ConnectionTrait>(
    db: &C,
    book_uuid: &str,
) -> Result<(), DbErr> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "DELETE FROM book_readings WHERE book_uuid = ?",
        [book_uuid.into()],
    ))
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::DatabaseConnection;

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
        record(&db, &id, ReadingChange::default()).await.unwrap();
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

        let owner = create_reader(&db, "Matthieu").await.unwrap();
        assert_eq!(status_seen(&db, &read).await, "read");
        assert_eq!(status_seen(&db, &wished).await, "wanting");

        let partner = create_reader(&db, "Claire").await.unwrap();
        assert_eq!(current_reader_id(&db).await.unwrap(), Some(partner.id));
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
        let owner = create_reader(&db, "Matthieu").await.unwrap();
        let partner = create_reader(&db, "Claire").await.unwrap();

        // Claire (current) finishes it and rates it.
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
        let owner = create_reader(&db, "Matthieu").await.unwrap();
        let partner = create_reader(&db, "Claire").await.unwrap();

        // Claire wishes for it: Matthieu, who has no reading of his own for
        // it, sees it wished too.
        set_status(&db, &id, "wanting").await;
        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "wanting");

        // Matthieu, looking at a wished book, starts reading it: it leaves
        // the wishlist for both, and Claire keeps her own status (none).
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
        let owner = create_reader(&db, "Matthieu").await.unwrap();
        let partner = create_reader(&db, "Claire").await.unwrap();
        set_status(&db, &id, "wanting").await;

        // Matthieu, who has his own reading of it, records that the household
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
        let owner = create_reader(&db, "Matthieu").await.unwrap();
        let partner = create_reader(&db, "Claire").await.unwrap();
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
        let owner = create_reader(&db, "Matthieu").await.unwrap();
        let partner = create_reader(&db, "Claire").await.unwrap();

        // Claire (current, no reading of her own) wishes for it.
        set_status(&db, &id, "wanting").await;
        assert_eq!(status_seen(&db, &id).await, "wanting");

        // Matthieu read it: his reading shows, the wish does not replace it.
        set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(status_seen(&db, &id).await, "read");

        // Matthieu moving his own reading leaves the household's wish alone.
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
        let owner = create_reader(&db, "Matthieu").await.unwrap();

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

        // A reader with a reading of their own files the book under that
        // reading, not under the wishlist.
        let partner = create_reader(&db, "Claire").await.unwrap();
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
        let list = crate::services::book_service::list_books(&db, wished())
            .await
            .unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn the_status_filter_follows_the_current_reader() {
        let db = migrated_db().await;
        let id = insert_book(&db, "Dune", "read").await;
        create_reader(&db, "Matthieu").await.unwrap();
        create_reader(&db, "Claire").await.unwrap();

        let read_filter = || crate::services::book_service::BookFilter {
            status: Some("read".to_owned()),
            ..Default::default()
        };
        let read = crate::services::book_service::list_books(&db, read_filter())
            .await
            .unwrap();
        assert!(read.is_empty(), "Claire has not read it");

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
        create_reader(&db, "Matthieu").await.unwrap();

        crate::services::book_service::delete_book(&db, &id)
            .await
            .unwrap();
        let view = current_view(&db, None).await.unwrap().unwrap();
        assert!(view.readings.is_empty());
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
        rename_reader(&db, &reader.id, "Claire").await.unwrap();
        assert_eq!(list_readers(&db).await.unwrap()[0].name, "Claire");
    }
}
