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

/// The five gap-fillable fields, in their canonical string names. These are the
/// only values accepted in the `field` column of the journal and the only
/// columns `apply_fill` may touch.
pub const FILL_FIELDS: [&str; 5] = [
    "summary",
    "publisher",
    "page_count",
    "publication_year",
    "cover_url",
];

/// Library completeness snapshot over **owned** books.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletenessStats {
    /// Total owned books.
    pub owned_total: i64,
    /// Owned books that have all five gap-fill fields populated.
    pub complete: i64,
    /// Owned books missing at least one gap-fill field.
    pub incomplete: i64,
    /// Owned, incomplete books that have no ISBN (not processable in V1).
    pub no_isbn: i64,
    /// Total empty gap-fill fields across all owned books (field-level progress,
    /// drops by exactly the number of fields filled). Max is `owned_total * 5`.
    pub empty_fields: i64,
}

/// A minimal book projection for selection and the "no ISBN" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteBook {
    pub id: i32,
    pub title: String,
    pub isbn: Option<String>,
}

/// An incomplete owned book with the precise set of fields still empty, for the
/// "books to complete" overview (manual completion entry point).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteBookDetail {
    pub id: i32,
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
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub page_count: Option<i32>,
    pub publication_year: Option<i32>,
    pub cover_url: Option<String>,
}

impl GapValues {
    /// True when every candidate field is empty (nothing to apply).
    pub fn is_empty(&self) -> bool {
        self.summary.is_none()
            && self.publisher.is_none()
            && self.page_count.is_none()
            && self.publication_year.is_none()
            && self.cover_url.is_none()
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
    /// Highest book id processed so far (monotonic; resume continues past it).
    pub cursor_book_id: i32,
    pub current_title: Option<String>,
}

/// A book in the "recently completed" list: the still-active (not undone)
/// fields this feature added to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFilledBook {
    pub book_id: i32,
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
    async fn list_incomplete_with_isbn(
        &self,
        after_id: i32,
        limit: u64,
    ) -> Result<Vec<IncompleteBook>, DomainError>;

    /// Count of owned, incomplete books that HAVE an ISBN (the run total).
    async fn count_incomplete_with_isbn(&self) -> Result<i64, DomainError>;

    /// Owned, incomplete books with NO ISBN (listed separately, not processed).
    async fn list_incomplete_without_isbn(&self) -> Result<Vec<IncompleteBook>, DomainError>;

    /// All owned, incomplete books with the exact fields still empty on each,
    /// ordered closest-to-complete first (fewest missing fields). For the manual
    /// completion overview. Capped at `limit`.
    async fn list_incomplete(&self, limit: u64) -> Result<Vec<IncompleteBookDetail>, DomainError>;

    /// Apply `candidate` to `book_id` `None`-only, in one transaction, writing a
    /// journal row per field actually written. Returns the fields written.
    async fn apply_fill(
        &self,
        batch_id: &str,
        book_id: i32,
        candidate: GapValues,
    ) -> Result<Vec<FilledField>, DomainError>;

    // ── Run lifecycle ──────────────────────────────────────────────────
    async fn create_run(&self, batch_id: &str, total: i64) -> Result<(), DomainError>;
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
    async fn undo_book(&self, batch_id: &str, book_id: i32) -> Result<usize, DomainError>;
    /// Revert all active entries of a whole batch. Returns reverted count.
    async fn undo_run(&self, batch_id: &str) -> Result<usize, DomainError>;
}
