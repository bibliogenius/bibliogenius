//! Household readers: one shared library, one reading state per person.
//!
//! Two people who share a library enrol their devices into the same account, so
//! account sync replicates the catalogue between them (ADR-044). The reading
//! state, though, is personal: one of them has read a book, the other has not.
//! `books` carries that state in columns, and a CRR replicates every column, so
//! it cannot hold two readers' answers. Three tables keep them beside it:
//!
//! - `readers` (CRR): the people of the household, `id` + display `name`.
//! - `book_readings` (CRR): one row per (book, reader) holding the reading
//!   status, the reading dates and the rating.
//! - `reader_local` (NOT a CRR): who reads on THIS device. A single row.
//!
//! **Opt-in, and a no-op until chosen.** A device with no current reader behaves
//! exactly as before: every rule here answers `None` or does nothing, and the
//! `books` columns stay the source of truth. Choosing a reader switches the
//! device to the per-reader state.
//!
//! **The `books` columns keep being written.** Every reading write lands on the
//! book row too (last writer wins), so the paths that still read the columns
//! directly (exports, peer payloads) keep working with a household-wide answer,
//! and a device without a reader keeps a coherent view.
//!
//! **The wishlist stays shared.** `wanting` is a household decision, not a
//! personal one, so it stays on `books.reading_status` alone and is never stored
//! in `book_readings`. The wish never hides a reading: a reader with a status of
//! their own for a wanted book sees that status, and moving it leaves the wish
//! in place. A reader with no status of their own sees `wanting`, and replacing
//! it takes the book off the wishlist for everyone.
//!
//! This file holds the types and the rules. [`HouseholdRepository`] splits in
//! two: the storage primitives an implementation provides, and the rules built
//! on them as provided methods, so every caller applies the same ones.
//! No framework dependencies (no SeaORM, no Axum).

use std::collections::HashMap;

use async_trait::async_trait;

use super::DomainError;
use crate::models::book::Book;

/// The status shared by the whole household (see the module docs).
pub const WANTING: &str = "wanting";

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
    /// The change puts the book on the wishlist (the row's status was not
    /// `wanting` and becomes it): the reader is named as the wisher. An edit
    /// that merely carries the shown `wanting` along claims nothing.
    pub wish_claimed: bool,
    /// The change takes the book off the wishlist: every reader's claim on
    /// it goes with it, or a later wish would name them again.
    pub wish_withdrawn: bool,
}

impl ReadingChange {
    /// Everything a create or a full update carries about the reading.
    pub fn from_book(book: &Book) -> Self {
        Self {
            reading_status: book.reading_status.clone(),
            started_reading_at: book.started_reading_at.clone(),
            finished_reading_at: book.finished_reading_at.clone(),
            user_rating: Some(book.user_rating),
            wish_claimed: false,
            wish_withdrawn: false,
        }
    }

    /// Set the wish flags from what the change does to the book row:
    /// `stored_status` before, `new_status` after, `keeps_wish` whether the
    /// row keeps `wanting` regardless (see `HouseholdRepository::keeps_wish`).
    pub fn with_wish_transition(
        mut self,
        stored_status: &str,
        new_status: &str,
        keeps_wish: bool,
    ) -> Self {
        self.wish_claimed = stored_status != WANTING && new_status == WANTING;
        self.wish_withdrawn = stored_status == WANTING && new_status != WANTING && !keeps_wish;
        self
    }

    fn apply_to(self, reading: &mut Reading) {
        // The status picker holds one value, so choosing the wish gives up
        // the reader's own status: kept, it would show in place of the wish.
        // The reader's row then says `wanting` when they made the wish (so
        // the household knows who wants the book) and nothing otherwise; the
        // wish itself lives on the book row.
        if let Some(status) = self.reading_status {
            reading.reading_status = if status != WANTING {
                status
            } else if self.wish_claimed || reading.reading_status == WANTING {
                WANTING.to_owned()
            } else {
                String::new()
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

/// Whether a reader's row holds a reading of their own. A `wanting` row only
/// names them as the wisher: the wish itself is the book row's.
fn is_own_status(status: &str) -> bool {
    !status.is_empty() && status != WANTING
}

/// The current reader's state for a set of books, ready to lay over `Book` DTOs.
#[derive(Debug, Clone)]
pub struct ReaderView {
    pub reader_id: String,
    readings: HashMap<String, Reading>,
    /// Names of the readers whose row says `wanting`, by book.
    wishers: HashMap<String, Vec<String>>,
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
        let stored_status = book.reading_status.take().unwrap_or_default();
        // The wish stays readable when the reader's own status takes its
        // place, and says who made it when the rows know.
        if stored_status == WANTING {
            book.wanted = Some(true);
            book.wished_by = self.wishers.get(id).filter(|w| !w.is_empty()).cloned();
        }
        let reading = self.reading_of(id, &stored_status);
        book.reading_status = Some(reading.reading_status);
        book.started_reading_at = Some(reading.started_reading_at);
        book.finished_reading_at = Some(reading.finished_reading_at);
        book.user_rating = reading.user_rating;
    }

    /// The current reader's state for one book, given the status stored on
    /// its row. For the read paths that do not go through a `Book` DTO.
    pub fn reading_of(&self, book_uuid: &str, stored_status: &str) -> Reading {
        let mut reading = self.readings.get(book_uuid).cloned().unwrap_or_default();
        if !is_own_status(&reading.reading_status) {
            // No reading of their own: the household's wish shows, or nothing.
            // A `wanting` row left behind by a wish since withdrawn says nothing.
            reading.reading_status = if stored_status == WANTING {
                WANTING.to_owned()
            } else {
                String::new()
            };
        }
        reading
    }

    pub fn apply_all(&self, books: &mut [Book]) {
        for book in books {
            self.apply(book);
        }
    }
}

/// The name as stored: trimmed, not empty, within [`MAX_READER_NAME_CHARS`].
pub fn valid_reader_name(name: &str) -> Result<&str, DomainError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DomainError::Validation(
            "Reader name is required".to_owned(),
        ));
    }
    if name.chars().count() > MAX_READER_NAME_CHARS {
        return Err(DomainError::Validation(format!(
            "Reader name is longer than {MAX_READER_NAME_CHARS} characters"
        )));
    }
    Ok(name)
}

/// Storage of the household readers and their readings, plus the rules built
/// on it. An implementation provides the primitives of the first half; the
/// provided methods of the second half are the only place the rules live.
#[async_trait]
pub trait HouseholdRepository: Send + Sync {
    // ── Storage primitives ───────────────────────────────────────────────

    /// Every reader of the household, oldest first.
    async fn list_readers(&self) -> Result<Vec<Reader>, DomainError>;

    async fn find_reader(&self, reader_id: &str) -> Result<Option<Reader>, DomainError>;

    /// Who reads on this device, if a reader was chosen.
    async fn current_reader_id(&self) -> Result<Option<String>, DomainError>;

    async fn store_current_reader_id(&self, reader_id: &str) -> Result<(), DomainError>;

    /// Put this device back on the shared book columns. The household and its
    /// readings are untouched.
    async fn clear_current_reader(&self) -> Result<(), DomainError>;

    async fn insert_reader(&self, reader: &Reader) -> Result<(), DomainError>;

    async fn store_reader_name(&self, reader_id: &str, name: &str) -> Result<(), DomainError>;

    /// Copy the reading state recorded on the book rows into `reader_id`'s
    /// readings. The shared wish is left on the rows.
    async fn inherit_book_readings(&self, reader_id: &str) -> Result<(), DomainError>;

    /// `reader_id`'s readings, keyed by book, for `book_ids` or for every book
    /// when `None`.
    async fn readings_of(
        &self,
        reader_id: &str,
        book_ids: Option<&[String]>,
    ) -> Result<HashMap<String, Reading>, DomainError>;

    /// Insert or replace `reader_id`'s reading of one book.
    async fn store_reading(
        &self,
        reader_id: &str,
        book_uuid: &str,
        reading: &Reading,
        exists: bool,
    ) -> Result<(), DomainError>;

    /// How many books `reader_id` has read, and, with `year`, finished that
    /// year. Readings of a book this device no longer holds are left out.
    async fn count_read(&self, reader_id: &str, year: Option<&str>) -> Result<i64, DomainError>;

    /// Forget who wished for `book_uuid`: every `wanting` row of the book
    /// goes back to no status.
    async fn clear_wish_claims(&self, book_uuid: &str) -> Result<(), DomainError>;

    async fn delete_reader(&self, reader_id: &str) -> Result<(), DomainError>;

    async fn delete_readings_of_reader(&self, reader_id: &str) -> Result<(), DomainError>;

    /// Names of the readers whose row says `wanting`, by book, oldest reader
    /// first, for `book_ids` or for every book when `None`.
    async fn wishers_of(
        &self,
        book_ids: Option<&[String]>,
    ) -> Result<HashMap<String, Vec<String>>, DomainError>;

    // ── Rules ────────────────────────────────────────────────────────────

    /// The current reader, resolved against the readers. A reader deleted on
    /// another device resolves to `None`, which puts this device back on the
    /// book columns.
    async fn current_reader(&self) -> Result<Option<Reader>, DomainError> {
        let Some(id) = self.current_reader_id().await? else {
            return Ok(None);
        };
        self.find_reader(&id).await
    }

    /// Add a reader to the household and make it the reader of this device.
    ///
    /// The FIRST reader of a household inherits the reading state already
    /// recorded on the books: it is the owner's history, typed before the
    /// household existed. Every later reader starts blank. "First" is judged
    /// on what this device has synced, so the owner should create their reader
    /// before the others enrol.
    ///
    /// Several writes: the caller runs it on a transaction.
    async fn create_reader(&self, name: &str) -> Result<Reader, DomainError> {
        let reader = Reader {
            id: crate::utils::uuid_gen::new_uuid_v7(),
            name: valid_reader_name(name)?.to_owned(),
        };
        let first = self.list_readers().await?.is_empty();
        self.insert_reader(&reader).await?;
        if first {
            self.inherit_book_readings(&reader.id).await?;
        }
        self.store_current_reader_id(&reader.id).await?;
        Ok(reader)
    }

    /// Make an existing reader the reader of this device.
    async fn set_current_reader(&self, reader_id: &str) -> Result<(), DomainError> {
        if !self.list_readers().await?.iter().any(|r| r.id == reader_id) {
            return Err(DomainError::NotFound);
        }
        self.store_current_reader_id(reader_id).await
    }

    /// Remove a reader and every reading of theirs. Their wishes stay on the
    /// books, unnamed. If this device had chosen them, it goes back to the
    /// shared columns. Several writes: the caller runs it on a transaction.
    async fn remove_reader(&self, reader_id: &str) -> Result<(), DomainError> {
        if self.find_reader(reader_id).await?.is_none() {
            return Err(DomainError::NotFound);
        }
        self.delete_readings_of_reader(reader_id).await?;
        self.delete_reader(reader_id).await?;
        if self.current_reader_id().await?.as_deref() == Some(reader_id) {
            self.clear_current_reader().await?;
        }
        Ok(())
    }

    async fn rename_reader(&self, reader_id: &str, name: &str) -> Result<(), DomainError> {
        self.store_reader_name(reader_id, valid_reader_name(name)?)
            .await
    }

    /// The current reader's state for `book_ids`, or for every book when
    /// `None`. `None` when this device has no reader: callers leave the books
    /// as stored.
    async fn current_view(
        &self,
        book_ids: Option<&[String]>,
    ) -> Result<Option<ReaderView>, DomainError> {
        let Some(reader_id) = self.current_reader().await?.map(|r| r.id) else {
            return Ok(None);
        };
        let readings = self.readings_of(&reader_id, book_ids).await?;
        let wishers = self.wishers_of(book_ids).await?;
        Ok(Some(ReaderView {
            reader_id,
            readings,
            wishers,
        }))
    }

    /// Lay the current reader's state over `books`, when this device has one.
    async fn overlay(&self, books: &mut [Book]) -> Result<(), DomainError> {
        let ids: Vec<String> = books.iter().filter_map(|b| b.id.clone()).collect();
        if let Some(view) = self.current_view(Some(&ids)).await? {
            view.apply_all(books);
        }
        Ok(())
    }

    /// Record `change` as the current reader's state for `book_uuid`. With no
    /// reader on the device only the withdrawal of the wish, which is the
    /// household's, leaves a trace.
    async fn record(&self, book_uuid: &str, change: ReadingChange) -> Result<(), DomainError> {
        if change.wish_withdrawn {
            self.clear_wish_claims(book_uuid).await?;
        }
        let Some(reader_id) = self.current_reader().await?.map(|r| r.id) else {
            return Ok(());
        };
        let ids = [book_uuid.to_owned()];
        let existing = self
            .readings_of(&reader_id, Some(&ids))
            .await?
            .remove(book_uuid);
        let exists = existing.is_some();
        let mut reading = existing.unwrap_or_default();
        change.apply_to(&mut reading);
        self.store_reading(&reader_id, book_uuid, &reading, exists)
            .await
    }

    /// Whether a status change must leave the household's wish on the book row
    /// instead of writing `new_status` over it. `stored_status` is the row's
    /// current status, `owned_after` whether the book is owned once the change
    /// is applied.
    ///
    /// True when the device has a reader holding a status of their own for a
    /// wished book: they were not looking at the wish, so moving their reading
    /// says nothing about it. A reader with no status of their own was shown
    /// the wish, and replacing it takes the book off the wishlist for all. So
    /// does anyone recording that the book is now owned.
    async fn keeps_wish(
        &self,
        book_uuid: &str,
        stored_status: &str,
        new_status: &str,
        owned_after: bool,
    ) -> Result<bool, DomainError> {
        if stored_status != WANTING || new_status == WANTING || owned_after {
            return Ok(false);
        }
        let ids = [book_uuid.to_owned()];
        let Some(view) = self.current_view(Some(&ids)).await? else {
            return Ok(false);
        };
        Ok(view
            .readings
            .get(book_uuid)
            .is_some_and(|reading| is_own_status(&reading.reading_status)))
    }

    /// How many books the current reader has read (finished in `year`, when
    /// given). `None` when this device has no reader: the caller counts the
    /// book columns.
    async fn count_read_by_current_reader(
        &self,
        year: Option<&str>,
    ) -> Result<Option<i64>, DomainError> {
        let Some(reader) = self.current_reader().await? else {
            return Ok(None);
        };
        Ok(Some(self.count_read(&reader.id, year).await?))
    }
}
