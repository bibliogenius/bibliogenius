//! Bulk metadata gap-fill repository trait and related types (ADR-041).
//!
//! This is the persistence contract for the "Compléter ma bibliothèque"
//! feature: a completeness statistic, selection of incomplete owned books, a
//! `None`-only apply step that records every field it writes in an undo
//! journal, and the run/journal lifecycle that makes a bulk run cancellable,
//! resumable and reversible.
//!
//! Invariants enforced by the implementation (NOT optional, see ADR-041):
//! - **`None` to `Some` only**: `apply_fill` never overwrites a populated field.
//! - **Safe rollback**: an undo reverts a field to empty ONLY if it still holds
//!   the exact value this feature wrote; if the user re-edited it since, the
//!   edit is left intact (the journal entry is marked superseded).

use async_trait::async_trait;

use super::DomainError;

/// The gap-fillable fields, in their canonical string names. These are the
/// only values accepted in the `field` column of the journal and the only
/// columns `apply_fill` may touch.
///
/// `title` is one of them even though it is the book's identity rather than
/// decoration: a book saved without a title is unreadable everywhere, and the
/// ISBN lookup that fills the other five knows it. It is also `NOT NULL` in
/// the schema, which is why the undo path reverts it to the empty string
/// instead of NULL.
pub const FILL_FIELDS: [&str; 6] = [
    "title",
    "summary",
    "publisher",
    "page_count",
    "publication_year",
    "cover_url",
];

/// Whitelist guard: `field` is one of the gap-fillable columns. The only
/// gate before a field name reaches SQL interpolation (selection predicate,
/// apply/undo statements), so every caller taking a field from the outside
/// must go through it.
pub fn is_fill_field(field: &str) -> bool {
    FILL_FIELDS.contains(&field)
}

/// The one column the journal may name that is NOT a gap-fill field: the ISBN.
///
/// It is filled by the "reimport to complete" mode (ADR-071), which reads it
/// off the file the library was imported from rather than from a lookup. It is
/// deliberately kept out of [`FILL_FIELDS`]: that list also defines what an
/// incomplete book is, and an ISBN-less book is not incomplete, it is
/// unidentifiable. `CompletenessStats::no_isbn` reports those on its own axis.
pub const JOURNAL_ONLY_FIELDS: [&str; 1] = ["isbn"];

/// Whitelist guard for the undo journal: every gap-fill field, plus the fields
/// only a reimport writes. Wider than [`is_fill_field`] on purpose, and applied
/// at the same place: before a field name reaches SQL interpolation.
pub fn is_journal_field(field: &str) -> bool {
    is_fill_field(field) || JOURNAL_ONLY_FIELDS.contains(&field)
}

/// How many owned books are still missing one given gap-fill field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldGap {
    /// One of [`FILL_FIELDS`].
    pub field: String,
    /// Owned books where that field is empty.
    pub missing: i64,
}

/// Library completeness snapshot over **owned** books.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletenessStats {
    /// Total owned books.
    pub owned_total: i64,
    /// Owned books that have every gap-fill field populated.
    pub complete: i64,
    /// Owned books missing at least one gap-fill field (a missing title
    /// included).
    pub incomplete: i64,
    /// Owned, incomplete books that have no ISBN (not processable in V1).
    pub no_isbn: i64,
    /// Total empty gap-fill fields across all owned books (field-level progress,
    /// drops by exactly the number of fields filled). Max is
    /// `owned_total * FILL_FIELDS.len()`. Equal to the sum of [`Self::gaps`].
    pub empty_fields: i64,
    /// Per-field breakdown of `empty_fields`, in [`FILL_FIELDS`] order. Exact
    /// over the whole library: the completeness screen shows these next to a
    /// field filter, where counting the (capped) overview list would
    /// under-report a large library.
    pub gaps: Vec<FieldGap>,
}

/// A minimal book projection for selection and the "no ISBN" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteBook {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
}

/// An incomplete owned book with the precise set of fields still empty, for the
/// "books to complete" overview (manual completion entry point).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteBookDetail {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    /// Subset of [`FILL_FIELDS`] that is currently empty on this book.
    pub missing: Vec<String>,
}

/// Candidate values from a metadata lookup. Each field is already `None` when
/// the lookup found nothing for it. `apply_fill` applies these `None`-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GapValues {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub page_count: Option<i32>,
    pub publication_year: Option<i32>,
    pub cover_url: Option<String>,
    /// Only a reimport (ADR-071) ever carries one: an ISBN lookup is keyed on
    /// the ISBN, so it has nothing to say about it. Journalled and undoable
    /// like the others, but not part of [`FILL_FIELDS`] (see
    /// [`JOURNAL_ONLY_FIELDS`]).
    pub isbn: Option<String>,
}

impl GapValues {
    /// True when every candidate field is empty (nothing to apply).
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.summary.is_none()
            && self.publisher.is_none()
            && self.page_count.is_none()
            && self.publication_year.is_none()
            && self.cover_url.is_none()
            && self.isbn.is_none()
    }
}

/// A field that `apply_fill` actually wrote, with the value written (string
/// form: integers are decimal-encoded). Used for telemetry and the undo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilledField {
    pub field: String,
    pub value: String,
}

/// Persisted state of a bulk run. Survives process restart so a run can resume
/// from `cursor_book_id` and progress can be polled after a relaunch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillRun {
    pub batch_id: String,
    /// `running` | `done` | `cancelled` | `interrupted`.
    pub status: String,
    pub total: i64,
    pub done: i64,
    /// Books that had at least one field filled.
    pub filled: i64,
    /// Books processed but with nothing to fill (no data or already complete).
    pub skipped: i64,
    /// Books whose lookup errored.
    pub errored: i64,
    /// Highest book uuid processed so far (lexicographically; resume continues
    /// past it). uuid v7 sorts ~chronologically so ordering still holds.
    pub cursor_book_id: String,
    pub current_title: Option<String>,
    /// Scope of the run: when set, one of [`FILL_FIELDS`], and the run only
    /// walks the owned books missing *that* field (it still fills every gap it
    /// finds on them). `None` walks the whole incomplete backlog. Persisted so
    /// a resume keeps the scope its cursor was built from.
    pub missing_field: Option<String>,
}

/// A book in the "recently completed" list: the still-active (not undone)
/// fields this feature added to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFilledBook {
    pub book_id: String,
    pub title: String,
    pub cover_url: Option<String>,
    /// Active journal entries for this book, newest run first.
    pub fields: Vec<RecentFilledField>,
}

/// One active journal entry surfaced in the undo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFilledField {
    pub journal_id: i64,
    pub batch_id: String,
    pub field: String,
    pub value: String,
}

/// Result of an undo request on a single journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoOutcome {
    /// The field still held our value and was reverted to empty.
    Reverted,
    /// The user re-edited the field since; left intact, entry retired.
    Superseded,
    /// No active journal entry matched.
    NotFound,
}

/// Persistence operations backing the bulk gap-fill feature (ADR-041).
#[async_trait]
pub trait MetadataFillRepository: Send + Sync {
    /// Completeness statistic over owned books.
    async fn completeness_stats(&self) -> Result<CompletenessStats, DomainError>;

    /// Owned, incomplete books that HAVE an ISBN, with `id > after_id`, ordered
    /// by `id`, capped at `limit`. Drives the self-draining work-list: once a
    /// book is filled it is no longer incomplete and drops out of this query.
    ///
    /// `missing_field` narrows the work-list to the books missing that one
    /// field (the run scope); it must pass [`is_fill_field`].
    async fn list_incomplete_with_isbn(
        &self,
        after_id: &str,
        limit: u64,
        missing_field: Option<&str>,
    ) -> Result<Vec<IncompleteBook>, DomainError>;

    /// Owned books with no cover for which a cover lookup already came back
    /// *conclusively* empty (`book_local.cover_lookup_failed_at`, written by
    /// the startup sweep when the sources answered and none of them carries a
    /// cover). This is the honest answer to "why did the run leave my covers
    /// empty": for these books there is nothing to fetch, and only a manual
    /// cover or a new source will change that.
    async fn count_covers_sources_have_not(&self) -> Result<i64, DomainError>;

    /// Count of owned, incomplete books that HAVE an ISBN (the run total),
    /// narrowed by `missing_field` the same way as the work-list.
    async fn count_incomplete_with_isbn(
        &self,
        missing_field: Option<&str>,
    ) -> Result<i64, DomainError>;

    /// Owned, incomplete books with NO ISBN (listed separately, not processed).
    async fn list_incomplete_without_isbn(&self) -> Result<Vec<IncompleteBook>, DomainError>;

    /// All owned, incomplete books with the exact fields still empty on each,
    /// ordered closest-to-complete first (fewest missing fields). For the manual
    /// completion overview. Capped at `limit`.
    ///
    /// The same two narrowings the overview offers as filters, applied here so
    /// the capped slice is drawn from the filtered set: `missing_field` (must
    /// pass [`is_fill_field`]) keeps the books missing that field, and
    /// `no_isbn_only` keeps the ones no automatic fill can identify. Without
    /// them the cap could hide every book a filter announces.
    async fn list_incomplete(
        &self,
        limit: u64,
        missing_field: Option<&str>,
        no_isbn_only: bool,
    ) -> Result<Vec<IncompleteBookDetail>, DomainError>;

    /// Apply `candidate` to `book_id` `None`-only, in one transaction, writing a
    /// journal row per field actually written. Returns the fields written.
    async fn apply_fill(
        &self,
        batch_id: &str,
        book_id: &str,
        candidate: GapValues,
    ) -> Result<Vec<FilledField>, DomainError>;

    // ── Run lifecycle ──────────────────────────────────────────────────
    async fn create_run(
        &self,
        batch_id: &str,
        total: i64,
        missing_field: Option<&str>,
    ) -> Result<(), DomainError>;
    /// The single run that is `running` or `interrupted`, if any.
    async fn get_active_run(&self) -> Result<Option<FillRun>, DomainError>;
    /// The most recent run of any status (for showing the last result).
    async fn last_run(&self) -> Result<Option<FillRun>, DomainError>;
    async fn get_run(&self, batch_id: &str) -> Result<Option<FillRun>, DomainError>;
    async fn update_run_progress(&self, run: &FillRun) -> Result<(), DomainError>;
    async fn set_run_status(&self, batch_id: &str, status: &str) -> Result<(), DomainError>;
    /// On startup: any leftover `running` run was interrupted by a kill; mark it
    /// `interrupted` so it can be offered as resumable rather than appearing live.
    async fn mark_running_as_interrupted(&self) -> Result<(), DomainError>;

    // ── Recently completed + undo ───────────────────────────────────────
    /// Books with active (not undone) journal entries, newest first, capped.
    async fn recent_filled(&self, limit: u64) -> Result<Vec<RecentFilledBook>, DomainError>;
    /// Revert a single journal entry (safe rule applies).
    async fn undo_field(&self, journal_id: i64) -> Result<UndoOutcome, DomainError>;
    /// Revert all active entries of one book in one batch. Returns reverted count.
    async fn undo_book(&self, batch_id: &str, book_id: &str) -> Result<usize, DomainError>;
    /// Revert all active entries of a whole batch. Returns reverted count.
    async fn undo_run(&self, batch_id: &str) -> Result<usize, DomainError>;
}
