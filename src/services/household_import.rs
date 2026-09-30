//! "Import my readings": merge another library's reading history into the
//! shared household library, for the current reader.
//!
//! Two people who each had a library before sharing one hold two catalogue
//! exports ("Exporter mon catalogue", the JSON `/api/export` writes). The
//! catalogue restore cannot merge them: it wipes the catalogue first, and on an
//! account-synced device that wipe replicates to every device. This import
//! writes nothing but additions and readings, book by book:
//!
//! - a book the shared library already holds (same ISBN in any of its forms,
//!   else the same title when the export has no ISBN) gets the current reader's
//!   status, dates and rating, and no duplicate;
//! - a book it does not hold is created, owned as in the export, with the
//!   reading;
//! - a book matching several rows is reported, never guessed.
//!
//! The export's `wanting` books join the shared wishlist only when they are new
//! to the library: a book the household already has is not pushed back into
//! the wishlist by an import.
//!
//! Idempotent: running the same file twice records the same readings again and
//! creates nothing the second time.

use std::collections::HashMap;

use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    sea_query::{Expr, Func},
};
use serde::Deserialize;

use crate::infrastructure::household::{self, ReadingChange};
use crate::models::Book;
use crate::models::book::{ActiveModel as BookActiveModel, Column, Entity as BookEntity};
use crate::services::book_service::{self, ServiceError};

/// How many ambiguous titles the report carries back for display.
const MAX_REPORTED_TITLES: usize = 50;

/// The part of a catalogue export this import reads. Every other section of
/// the file (copies, loans, gamification...) is ignored.
#[derive(Debug, Deserialize)]
struct CatalogueExport {
    #[serde(default)]
    books: Vec<ExportedBook>,
    #[serde(default)]
    authors: Vec<ExportedAuthor>,
    #[serde(default)]
    book_authors: Vec<ExportedBookAuthor>,
}

#[derive(Debug, Deserialize)]
struct ExportedBook {
    #[serde(default)]
    id: Option<String>,
    title: String,
    #[serde(default)]
    isbn: Option<String>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    publication_year: Option<i32>,
    #[serde(default)]
    reading_status: Option<String>,
    #[serde(default)]
    started_reading_at: Option<String>,
    #[serde(default)]
    finished_reading_at: Option<String>,
    #[serde(default)]
    user_rating: Option<i32>,
    #[serde(default)]
    owned: Option<bool>,
    /// The simplified export carries the author inline.
    #[serde(default)]
    author: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExportedAuthor {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct ExportedBookAuthor {
    book_id: String,
    author_id: String,
}

/// What the import did, for the summary shown to the reader.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReadingImportReport {
    /// Books already in the library that received the reading.
    pub matched: usize,
    /// Books added to the library, with the reading.
    pub created: usize,
    /// Rows matching several books: nothing written.
    pub ambiguous: usize,
    /// Up to [`MAX_REPORTED_TITLES`] titles of the ambiguous rows.
    pub ambiguous_titles: Vec<String>,
    /// Rows unusable as they stand (no title) or refused by validation.
    pub skipped: usize,
}

/// A status the library accepts; anything else in the file becomes "no status"
/// rather than failing the row.
fn accepted_status(status: Option<String>) -> String {
    match status {
        Some(s)
            if s == crate::models::book::NO_READING_STATUS
                || crate::models::book::READING_STATUSES.contains(&s.as_str()) =>
        {
            s
        }
        _ => crate::models::book::NO_READING_STATUS.to_owned(),
    }
}

/// The library rows this exported book may be: by ISBN when it has one, else
/// by title (case-insensitive).
async fn candidates(
    db: &DatabaseConnection,
    book: &ExportedBook,
) -> Result<Vec<crate::models::book::Model>, ServiceError> {
    if let Some(isbn) = book_service::normalize_isbn(book.isbn.clone()) {
        return Ok(BookEntity::find()
            .filter(book_service::stored_isbn_matches(
                crate::utils::isbn::lookup_forms(&isbn),
            ))
            .all(db)
            .await?);
    }
    let title = book.title.trim();
    if title.is_empty() {
        return Ok(Vec::new());
    }
    Ok(BookEntity::find()
        .filter(Expr::expr(Func::lower(Expr::col(Column::Title))).eq(title.to_lowercase()))
        .all(db)
        .await?)
}

/// Merge the readings of a catalogue export into the library, for the current
/// household reader. Refused when this device has no reader: the readings would
/// have nobody to belong to.
pub async fn import_readings(
    db: &DatabaseConnection,
    json: &str,
) -> Result<ReadingImportReport, ServiceError> {
    if household::current_reader(db).await?.is_none() {
        return Err(ServiceError::InvalidInput(
            "Choose who reads on this device before importing readings".to_owned(),
        ));
    }
    let export: CatalogueExport = serde_json::from_str(json)
        .map_err(|e| ServiceError::InvalidInput(format!("Unreadable catalogue export: {e}")))?;

    // Author names per exported book id, in file order.
    let author_names: HashMap<&str, &str> = export
        .authors
        .iter()
        .map(|a| (a.id.as_str(), a.name.as_str()))
        .collect();
    let mut authors_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for link in &export.book_authors {
        if let Some(name) = author_names.get(link.author_id.as_str()) {
            authors_of
                .entry(link.book_id.as_str())
                .or_default()
                .push(name);
        }
    }

    let mut report = ReadingImportReport::default();
    for exported in &export.books {
        if exported.title.trim().is_empty() {
            report.skipped += 1;
            continue;
        }
        let status = accepted_status(exported.reading_status.clone());
        let found = candidates(db, exported).await?;

        match found.as_slice() {
            [] => {
                let author = exported.author.clone().or_else(|| {
                    exported
                        .id
                        .as_deref()
                        .and_then(|id| authors_of.get(id))
                        .map(|names| names.join(", "))
                });
                let created = book_service::create_book(
                    db,
                    Book {
                        title: exported.title.clone(),
                        isbn: exported.isbn.clone(),
                        author,
                        publisher: exported.publisher.clone(),
                        publication_year: exported.publication_year,
                        reading_status: Some(status),
                        started_reading_at: Some(exported.started_reading_at.clone()),
                        finished_reading_at: Some(exported.finished_reading_at.clone()),
                        user_rating: exported.user_rating,
                        owned: Some(exported.owned.unwrap_or(true)),
                        ..Default::default()
                    },
                )
                .await;
                match created {
                    Ok(_) => report.created += 1,
                    Err(ServiceError::InvalidInput(_)) => report.skipped += 1,
                    Err(e) => return Err(e),
                }
            }
            [model] => {
                // The household's wishlist is not re-entered by an import: a
                // book the library holds keeps its shared status.
                let change = ReadingChange {
                    reading_status: (status != "wanting").then(|| status.clone()),
                    started_reading_at: Some(exported.started_reading_at.clone()),
                    finished_reading_at: Some(exported.finished_reading_at.clone()),
                    user_rating: Some(exported.user_rating),
                };
                // The book columns keep the last writer's reading, as every
                // household write does (see `infrastructure::household`).
                let mut active: BookActiveModel = model.clone().into();
                if let Some(status) = &change.reading_status {
                    active.reading_status = Set(status.clone());
                }
                active.started_reading_at = Set(exported.started_reading_at.clone());
                active.finished_reading_at = Set(exported.finished_reading_at.clone());
                active.user_rating = Set(exported.user_rating);
                active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                active.update(db).await?;
                household::record(db, &model.id, change).await?;
                let _ = crate::sync::log_operation(db, "book", &model.id, "UPDATE", None).await;
                report.matched += 1;
            }
            _ => {
                report.ambiguous += 1;
                if report.ambiguous_titles.len() < MAX_REPORTED_TITLES {
                    report.ambiguous_titles.push(exported.title.clone());
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn migrated_db() -> DatabaseConnection {
        crate::db::init_db("sqlite::memory:")
            .await
            .expect("init db")
    }

    async fn add_book(db: &DatabaseConnection, title: &str, isbn: &str, status: &str) -> String {
        book_service::create_book(
            db,
            Book {
                title: title.to_owned(),
                isbn: Some(isbn.to_owned()),
                reading_status: Some(status.to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id
        .unwrap()
    }

    async fn status_of(db: &DatabaseConnection, id: &str) -> Book {
        book_service::get_book(db, id).await.unwrap()
    }

    const EXPORT: &str = r#"{
        "version": "1",
        "books": [
            {"id": "x-1", "title": "Dune", "isbn": "2-266-32034-3",
             "reading_status": "read", "user_rating": 9,
             "finished_reading_at": "2024-05-01", "owned": true},
            {"id": "x-2", "title": "Hyperion", "isbn": "9782266111560",
             "reading_status": "to_read", "owned": true},
            {"id": "x-3", "title": "Ambigu", "isbn": "9780000000002",
             "reading_status": "read", "owned": true},
            {"id": "x-4", "title": "   ", "reading_status": "read"}
        ],
        "authors": [{"id": "a-1", "name": "Dan Simmons"}],
        "book_authors": [{"book_id": "x-2", "author_id": "a-1"}],
        "copies": [], "loans": []
    }"#;

    #[tokio::test]
    async fn merges_readings_without_duplicating_the_shared_books() {
        let db = migrated_db().await;
        // The shared library: Dune under its ISBN-13, and a duplicated ISBN.
        let dune = add_book(&db, "Dune", "9782266320344", "to_read").await;
        add_book(&db, "Ambigu 1", "9780000000002", "to_read").await;
        add_book(&db, "Ambigu 2", "978-0-00-000000-2", "to_read").await;

        let owner = household::create_reader(&db, "Matthieu").await.unwrap();
        household::create_reader(&db, "Claire").await.unwrap();

        let report = import_readings(&db, EXPORT).await.unwrap();
        assert_eq!(report.matched, 1, "Dune, matched across ISBN-10/13");
        assert_eq!(report.created, 1, "Hyperion, new to the library");
        assert_eq!(report.ambiguous, 1);
        assert_eq!(report.ambiguous_titles, vec!["Ambigu".to_owned()]);
        assert_eq!(report.skipped, 1, "the title-less row");

        let hers = status_of(&db, &dune).await;
        assert_eq!(hers.reading_status.as_deref(), Some("read"));
        assert_eq!(hers.user_rating, Some(9));
        assert_eq!(
            hers.finished_reading_at,
            Some(Some("2024-05-01".to_owned()))
        );

        let all = book_service::list_books(&db, Default::default())
            .await
            .unwrap();
        assert_eq!(all.len(), 4, "3 shared books + Hyperion, no duplicate");
        let hyperion = all.iter().find(|b| b.title == "Hyperion").unwrap();
        assert_eq!(hyperion.author.as_deref(), Some("Dan Simmons"));
        assert_eq!(hyperion.reading_status.as_deref(), Some("to_read"));

        // Matthieu's own reading of Dune is untouched.
        household::set_current_reader(&db, &owner.id).await.unwrap();
        assert_eq!(
            status_of(&db, &dune).await.reading_status.as_deref(),
            Some("to_read")
        );
    }

    #[tokio::test]
    async fn running_it_twice_creates_nothing_more() {
        let db = migrated_db().await;
        household::create_reader(&db, "Claire").await.unwrap();

        let first = import_readings(&db, EXPORT).await.unwrap();
        let second = import_readings(&db, EXPORT).await.unwrap();
        assert_eq!(first.created, 3);
        assert_eq!(second.created, 0);
        assert_eq!(second.matched, 3);
    }

    #[tokio::test]
    async fn refused_without_a_reader() {
        let db = migrated_db().await;
        assert!(matches!(
            import_readings(&db, EXPORT).await,
            Err(ServiceError::InvalidInput(_))
        ));
    }
}
