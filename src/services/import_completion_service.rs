//! "Reimport to complete": filling a library that was imported without ISBNs
//! from the very file it came from (ADR-071).
//!
//! A file whose ISBN column was not recognised produced a shelf of books with
//! titles, authors, and nothing to identify them by. Re-importing that file
//! would duplicate the shelf. This module reads the same rows a second time and
//! writes what is missing into the books that are already there:
//!
//! - each row and each book is reduced to the `ta:` half of
//!   [`crate::utils::dedup_key::book_dedup_key`], the key ADR-070 already owns;
//! - a key that is not unique on either side matches nothing, and the row is
//!   reported rather than guessed;
//! - what is written goes through [`MetadataFillRepository::apply_fill`], so it
//!   is `None`-only, journalled, and reversible by batch.
//!
//! Nothing here creates a book, overwrites a value, or reaches the network.

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, QuerySelect, Statement};

use crate::domain::DomainError;
use crate::domain::metadata_fill::GapValues;
use crate::infrastructure::AppState;
use crate::models::book;
use crate::utils::dedup_key::book_dedup_key;

/// How many skipped rows the report carries back for display. The counts are
/// exact; only the list is bounded, so a 3000-row file cannot inflate the
/// payload that crosses the FFI boundary.
const MAX_REPORTED_ROWS: usize = 200;

/// One row of the reimported file, already parsed by the Dart import readers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRow {
    pub title: String,
    /// The author cell as the file gives it, undivided: the import stored it
    /// that way too, which is what makes the match exact rather than fuzzy.
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
}

/// A book as the matcher sees it: enough to compute its keys, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryBook {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
    pub publication_year: Option<i32>,
    /// Every linked author name, alphabetically sorted.
    pub authors: Vec<String>,
}

/// Why a row filled nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// No book carries this row's title, author and year.
    NoMatch,
    /// The file itself holds this key more than once.
    AmbiguousInFile,
    /// Several books carry this key.
    AmbiguousInLibrary,
}

impl SkipReason {
    /// Stable wire name, read by the summary screen.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NoMatch => "no_match",
            SkipReason::AmbiguousInFile => "ambiguous_in_file",
            SkipReason::AmbiguousInLibrary => "ambiguous_in_library",
        }
    }
}

/// A row the campaign did not use, with the reason, for the summary lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRow {
    pub title: String,
    pub author: Option<String>,
    pub reason: SkipReason,
}

/// What one row resolved to, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowMatch {
    /// The row matched exactly one book.
    Book(String),
    /// The row filled nothing, and why.
    Skipped(SkipReason),
    /// The library already holds this row's ISBN. Nothing to fill and nothing
    /// to report: a book that has its ISBN is not addressable by a `ta:` key
    /// (D4), and calling that "no match" would tell someone whose library is
    /// perfectly complete that the file matches nothing.
    AlreadyPresent,
}

/// Outcome of a whole campaign.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionReport {
    /// Undo handle: `metadata_fill_undo_run(batch_id)` reverts the campaign.
    pub batch_id: String,
    pub rows_read: i64,
    /// Books that received at least one field.
    pub completed: i64,
    pub fields_written: i64,
    pub no_match: i64,
    /// Rows refused for ambiguity, on either side.
    pub ambiguous: i64,
    /// A bounded sample of the skipped rows, for the two consultable lists.
    pub skipped: Vec<SkippedRow>,
}

/// The largest group of ISBN-less owned books added on a single day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoIsbnCluster {
    /// `YYYY-MM-DD`, the day those books were inserted.
    pub day: String,
    pub count: i64,
}

/// One index entry: the book carrying a key, or the fact that several do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexEntry {
    One(String),
    Many,
}

/// The `ta:` keys a book answers to.
///
/// One per linked author name, plus one on the joined form, because the app's
/// own catalogue export writes several authors in a single cell. A book with no
/// author still gets its authorless key. Duplicates are collapsed: a
/// single-author book yields one key, not two identical ones.
fn library_keys(b: &LibraryBook) -> Vec<String> {
    let mut variants: Vec<Option<String>> = b.authors.iter().cloned().map(Some).collect();
    if b.authors.len() > 1 {
        variants.push(Some(b.authors.join(", ")));
    }
    if variants.is_empty() {
        variants.push(None);
    }
    let mut keys: Vec<String> = variants
        .into_iter()
        .map(|author| {
            // No ISBN on this side either: a book that has one is filtered out
            // before it reaches here, so the key is always the `ta:` form.
            book_dedup_key(None, &b.title, author.as_deref(), b.publication_year)
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// The key one file row answers to. The row's ISBN is the value being written,
/// never part of the identity.
fn row_key(row: &ImportRow) -> String {
    book_dedup_key(
        None,
        &row.title,
        row.author.as_deref(),
        row.publication_year,
    )
}

/// Index the library by key. Books that already carry a usable ISBN are left
/// out: their identity is the ISBN, no file row can address them, and this
/// feature has nothing to add to them.
fn build_index(books: &[LibraryBook]) -> HashMap<String, IndexEntry> {
    let mut index: HashMap<String, IndexEntry> = HashMap::new();
    for b in books {
        if b.isbn.as_deref().map(str::trim).unwrap_or("").is_empty() {
            for key in library_keys(b) {
                index
                    .entry(key)
                    .and_modify(|e| {
                        // The same book reaching one key twice is not an
                        // ambiguity; two different books are.
                        if !matches!(e, IndexEntry::One(id) if *id == b.id) {
                            *e = IndexEntry::Many;
                        }
                    })
                    .or_insert_with(|| IndexEntry::One(b.id.clone()));
            }
        }
    }
    index
}

/// Resolve every row against the library, without writing anything.
///
/// Pure and total: same rows and same books give the same answer, in the same
/// order as `rows`.
pub fn match_rows(rows: &[ImportRow], books: &[LibraryBook]) -> Vec<RowMatch> {
    let index = build_index(books);
    // Every ISBN the library already holds, to tell "this book is done" from
    // "no such book" when a row finds no key. Both sides go through the same
    // normalization the writes use, or a hyphenated value on either side would
    // read as a book this library does not have.
    let known_isbns: std::collections::HashSet<String> = books
        .iter()
        .filter_map(|b| crate::services::book_service::normalize_isbn(b.isbn.clone()))
        .collect();
    // Each row's key is normalized once and read twice: to count how many rows
    // share it, then to resolve it.
    let keys: Vec<String> = rows.iter().map(row_key).collect();

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for key in &keys {
        *seen.entry(key.as_str()).or_insert(0) += 1;
    }

    rows.iter()
        .zip(&keys)
        .map(|(row, key)| {
            if seen.get(key.as_str()).copied().unwrap_or(0) > 1 {
                return RowMatch::Skipped(SkipReason::AmbiguousInFile);
            }
            match index.get(key) {
                Some(IndexEntry::One(id)) => RowMatch::Book(id.clone()),
                Some(IndexEntry::Many) => RowMatch::Skipped(SkipReason::AmbiguousInLibrary),
                None if crate::services::book_service::normalize_isbn(row.isbn.clone())
                    .is_some_and(|i| known_isbns.contains(&i)) =>
                {
                    RowMatch::AlreadyPresent
                }
                None => RowMatch::Skipped(SkipReason::NoMatch),
            }
        })
        .collect()
}

/// Load the library as the matcher needs it: every book, with its authors.
///
/// Not restricted to owned books. A file that carried wishlist rows imported
/// them as such, and they lost their ISBN the same way.
async fn load_library(db: &DatabaseConnection) -> Result<Vec<LibraryBook>, DomainError> {
    let rows: Vec<(String, String, Option<String>, Option<i32>)> = book::Entity::find()
        .select_only()
        .column(book::Column::Id)
        .column(book::Column::Title)
        .column(book::Column::Isbn)
        .column(book::Column::PublicationYear)
        .into_tuple()
        .all(db)
        .await
        .map_err(|e| DomainError::Database(e.to_string()))?;

    let mut authors = crate::services::author_names::author_names_by_book(db).await?;

    Ok(rows
        .into_iter()
        .map(|(id, title, isbn, publication_year)| LibraryBook {
            authors: authors.remove(&id).unwrap_or_default(),
            id,
            title,
            isbn,
            publication_year,
        })
        .collect())
}

/// Run a campaign: match every row, then fill the matched books `None`-only
/// under one batch id.
pub async fn complete_from_rows(
    state: &AppState,
    rows: Vec<ImportRow>,
) -> Result<CompletionReport, DomainError> {
    let db = state.db();
    let repo = state.metadata_fill_repo.clone();
    let books = load_library(db).await?;
    let matches = match_rows(&rows, &books);

    let batch_id = uuid::Uuid::new_v4().to_string();
    let mut report = CompletionReport {
        batch_id: batch_id.clone(),
        rows_read: rows.len() as i64,
        ..Default::default()
    };

    for (row, outcome) in rows.iter().zip(matches) {
        match outcome {
            // Counted nowhere, on purpose: the book is already identified.
            RowMatch::AlreadyPresent => {}
            RowMatch::Skipped(reason) => {
                match reason {
                    SkipReason::NoMatch => report.no_match += 1,
                    SkipReason::AmbiguousInFile | SkipReason::AmbiguousInLibrary => {
                        report.ambiguous += 1
                    }
                }
                if report.skipped.len() < MAX_REPORTED_ROWS {
                    report.skipped.push(SkippedRow {
                        title: row.title.clone(),
                        author: row.author.clone(),
                        reason,
                    });
                }
            }
            RowMatch::Book(book_id) => {
                let candidate = GapValues {
                    // Through the same normalization `create_book` applies: a
                    // hyphenated ISBN stored as such matches nothing, and this
                    // is a second write path into the same column.
                    isbn: crate::services::book_service::normalize_isbn(row.isbn.clone()),
                    publisher: row.publisher.clone(),
                    publication_year: row.publication_year,
                    ..Default::default()
                };
                if candidate.is_empty() {
                    continue;
                }
                let filled = repo.apply_fill(&batch_id, &book_id, candidate).await?;
                if !filled.is_empty() {
                    report.completed += 1;
                    report.fields_written += filled.len() as i64;
                }
            }
        }
    }

    Ok(report)
}

/// The largest same-day group of owned books with no ISBN, or `None` when the
/// library has none.
///
/// One of the two signals that offer the reimport (ADR-071 D9). A bulk import
/// stamps every row with its insertion time, so it leaves a dense cluster; a
/// shelf built scan after scan does not.
pub async fn no_isbn_cluster(
    db: &DatabaseConnection,
) -> Result<Option<NoIsbnCluster>, DomainError> {
    let sql = "SELECT SUBSTR(created_at, 1, 10) AS day, COUNT(*) AS n \
         FROM books WHERE owned = 1 AND (isbn IS NULL OR TRIM(isbn) = '') \
         GROUP BY day ORDER BY n DESC, day DESC LIMIT 1";
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            sql.to_owned(),
        ))
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let day = row.try_get::<String>("", "day")?;
    let count = row.try_get::<i64>("", "n")?;
    Ok((count > 0).then_some(NoIsbnCluster { day, count }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str, author: Option<&str>, year: Option<i32>, isbn: Option<&str>) -> ImportRow {
        ImportRow {
            title: title.to_string(),
            author: author.map(str::to_string),
            isbn: isbn.map(str::to_string),
            publisher: None,
            publication_year: year,
        }
    }

    fn book(id: &str, title: &str, authors: &[&str], year: Option<i32>) -> LibraryBook {
        LibraryBook {
            id: id.to_string(),
            title: title.to_string(),
            isbn: None,
            publication_year: year,
            authors: authors.iter().map(|a| a.to_string()).collect(),
        }
    }

    /// The ordinary case: one row, one book, written by the same import.
    #[test]
    fn a_row_matches_the_book_that_import_created() {
        let rows = [row(
            "Martin Eden",
            Some("Jack London"),
            Some(1909),
            Some("x"),
        )];
        let books = [book("b1", "Martin Eden", &["Jack London"], Some(1909))];
        assert_eq!(
            match_rows(&rows, &books),
            vec![RowMatch::Book("b1".to_string())]
        );
    }

    /// Case, punctuation and spacing differ between the file and the stored
    /// value on a library that has been edited by hand: the key normalizes both.
    #[test]
    fn matching_ignores_case_and_punctuation() {
        let rows = [row("l'étranger!", Some("albert  camus"), Some(1942), None)];
        let books = [book("b1", "L'Étranger", &["Albert Camus"], Some(1942))];
        assert_eq!(
            match_rows(&rows, &books),
            vec![RowMatch::Book("b1".to_string())]
        );
    }

    /// The app's own export joins authors in one cell. Both the joined form and
    /// a single name must find the book (ADR-071 D3).
    #[test]
    fn a_multi_author_book_answers_to_the_joined_cell_and_to_each_name() {
        let books = [book("b1", "Le Horla", &["Dumas", "Hugo"], Some(1887))];
        let joined = [row("Le Horla", Some("Dumas, Hugo"), Some(1887), None)];
        let single = [row("Le Horla", Some("Hugo"), Some(1887), None)];
        assert_eq!(
            match_rows(&joined, &books),
            vec![RowMatch::Book("b1".to_string())]
        );
        assert_eq!(
            match_rows(&single, &books),
            vec![RowMatch::Book("b1".to_string())]
        );
    }

    /// Two books sharing a key are never disambiguated by picking one.
    #[test]
    fn two_books_on_one_key_refuse_the_row() {
        let rows = [row("Dune", Some("Herbert"), Some(1965), Some("x"))];
        let books = [
            book("b1", "Dune", &["Herbert"], Some(1965)),
            book("b2", "Dune", &["Herbert"], Some(1965)),
        ];
        assert_eq!(
            match_rows(&rows, &books),
            vec![RowMatch::Skipped(SkipReason::AmbiguousInLibrary)]
        );
    }

    /// The same ambiguity, on the file side.
    #[test]
    fn two_rows_on_one_key_refuse_both() {
        let rows = [
            row("Dune", Some("Herbert"), Some(1965), Some("a")),
            row("Dune", Some("Herbert"), Some(1965), Some("b")),
        ];
        let books = [book("b1", "Dune", &["Herbert"], Some(1965))];
        assert_eq!(
            match_rows(&rows, &books),
            vec![
                RowMatch::Skipped(SkipReason::AmbiguousInFile),
                RowMatch::Skipped(SkipReason::AmbiguousInFile),
            ]
        );
    }

    /// A book that already has an ISBN is not in the index at all (D4): the row
    /// is reported as unmatched, never as a conflict.
    #[test]
    fn a_book_that_already_has_an_isbn_is_not_addressable() {
        let rows = [row(
            "Dune",
            Some("Herbert"),
            Some(1965),
            Some("9780441013593"),
        )];
        let mut owned = book("b1", "Dune", &["Herbert"], Some(1965));
        owned.isbn = Some("9780441172719".to_string());
        assert_eq!(
            match_rows(&rows, &[owned]),
            vec![RowMatch::Skipped(SkipReason::NoMatch)],
            "a different ISBN is an edition this library does not hold",
        );
    }

    /// The row's own ISBN is already somewhere in the library: nothing to fill,
    /// and nothing to report. A Gleeph export reimported onto the library it
    /// came from is exactly this, nine rows out of nine, and reading "9 without
    /// a match" on a library that is perfectly complete is a false alarm.
    #[test]
    fn a_row_whose_isbn_the_library_already_holds_is_not_reported() {
        let rows = [row("Fables", None, None, Some("9782253010043"))];
        let mut owned = book("b1", "Fables", &[], None);
        owned.isbn = Some("9782253010043".to_string());
        assert_eq!(match_rows(&rows, &[owned]), vec![RowMatch::AlreadyPresent]);
    }

    /// The same, with a hyphenated value on the library side: an older row can
    /// carry one, and the two sides must be compared in the same form.
    #[test]
    fn already_present_compares_isbns_in_their_normalized_form() {
        let rows = [row("Martin Eden", None, None, Some("9782264024848"))];
        let mut owned = book("b1", "Martin Eden", &[], None);
        owned.isbn = Some("978-2-264-02484-8".to_string());
        assert_eq!(match_rows(&rows, &[owned]), vec![RowMatch::AlreadyPresent]);
    }

    /// A blank ISBN is an absence, not an identity (migration 057).
    #[test]
    fn a_blank_isbn_still_leaves_the_book_addressable() {
        let rows = [row("Dune", Some("Herbert"), Some(1965), Some("978"))];
        let mut owned = book("b1", "Dune", &["Herbert"], Some(1965));
        owned.isbn = Some("   ".to_string());
        assert_eq!(
            match_rows(&rows, &[owned]),
            vec![RowMatch::Book("b1".to_string())]
        );
    }

    /// A year on one side only changes the key, so the row does not match. The
    /// summary says so rather than filling the closest book.
    #[test]
    fn a_year_missing_on_one_side_does_not_match() {
        let rows = [row("Dune", Some("Herbert"), None, Some("x"))];
        let books = [book("b1", "Dune", &["Herbert"], Some(1965))];
        assert_eq!(
            match_rows(&rows, &books),
            vec![RowMatch::Skipped(SkipReason::NoMatch)]
        );
    }

    /// The value written goes through the same normalization as a book the
    /// import creates: `books.isbn` holds digits, and a hyphenated one there
    /// matches nothing (the wishlist join, the dedup key, the peer lookup).
    #[test]
    fn a_hyphenated_isbn_is_normalized_before_it_is_written() {
        assert_eq!(
            crate::services::book_service::normalize_isbn(Some("978-2-264-02484-8".to_string()))
                .as_deref(),
            Some("9782264024848"),
        );
    }

    /// A book with no author at all is still addressable by a row with no
    /// author cell: both sides produce the same authorless key.
    #[test]
    fn an_authorless_book_matches_an_authorless_row() {
        let rows = [row("Carnet", None, Some(2020), Some("x"))];
        let books = [book("b1", "Carnet", &[], Some(2020))];
        assert_eq!(
            match_rows(&rows, &books),
            vec![RowMatch::Book("b1".to_string())]
        );
    }
}
