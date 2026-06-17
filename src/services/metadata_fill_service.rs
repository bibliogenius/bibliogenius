//! Bulk metadata gap-fill orchestration (ADR-041) — "Compléter ma bibliothèque".
//!
//! A single background run iterates owned, incomplete books that have an ISBN,
//! calls the existing [`lookup_service::lookup_metadata_by_isbn`] per book, and
//! applies the result `None`-only via [`MetadataFillRepository::apply_fill`],
//! recording every written field in the undo journal.
//!
//! The run is:
//! - **throttled & polite**: concurrency 1, a base inter-book delay with jitter,
//!   and an adaptive backoff that widens the delay after consecutive empty/error
//!   lookups (OpenLibrary / Inventaire answer 403/429 when hammered, ADR-040).
//! - **cancellable**: a shared flag checked every iteration.
//! - **resumable**: the work-list is self-draining (a filled book stops being
//!   incomplete), and a persisted cursor avoids re-hitting books already tried
//!   this run, so a kill/restart continues where it left off.
//!
//! Layering: this service depends only on the repository trait and the lookup
//! service; the concrete repo and the run manager are injected via `AppState`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sea_orm::DatabaseConnection;

use crate::domain::metadata_fill::{
    CompletenessStats, FillRun, GapValues, IncompleteBook, IncompleteBookDetail,
    MetadataFillRepository, RecentFilledBook, UndoOutcome,
};
use crate::infrastructure::AppState;
use crate::openlibrary::BookMetadata;

/// Page size for the incomplete-book work-list query.
const PAGE: u64 = 50;
/// Base polite delay between per-book lookups.
const BASE_DELAY_MS: u64 = 1000;
/// Hard cap for the adaptive backoff delay.
const MAX_DELAY_MS: u64 = 30_000;
/// Consecutive empty/error lookups tolerated before the delay starts widening.
const BACKOFF_AFTER: u32 = 3;

/// In-memory guard ensuring a single bulk run at a time and carrying the
/// cancellation flag for the active run. Progress itself lives in the run table
/// (single source of truth, survives restart) — this only tracks liveness.
#[derive(Default)]
pub struct MetadataFillManager {
    inner: std::sync::Mutex<ManagerInner>,
}

#[derive(Default)]
struct ManagerInner {
    active_batch: Option<String>,
    cancel: Option<Arc<AtomicBool>>,
}

impl MetadataFillManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim the manager for `batch_id`. Returns the cancellation flag, or `None`
    /// if a run is already in progress.
    fn try_begin(&self, batch_id: String) -> Option<Arc<AtomicBool>> {
        let mut g = self.inner.lock().unwrap();
        if g.active_batch.is_some() {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        g.active_batch = Some(batch_id);
        g.cancel = Some(flag.clone());
        Some(flag)
    }

    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().active_batch.is_some()
    }

    pub fn active_batch(&self) -> Option<String> {
        self.inner.lock().unwrap().active_batch.clone()
    }

    /// Request cancellation of the active run (no-op if nothing is running).
    pub fn request_cancel(&self) {
        if let Some(flag) = &self.inner.lock().unwrap().cancel {
            flag.store(true, Ordering::SeqCst);
        }
    }

    fn finish(&self) {
        let mut g = self.inner.lock().unwrap();
        g.active_batch = None;
        g.cancel = None;
    }
}

fn err_to_string<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Extract the first 4-digit year from a free-form date/year string.
fn parse_year(raw: &str) -> Option<i32> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4].iter().all(|b| b.is_ascii_digit()) {
            return raw[i..i + 4].parse::<i32>().ok();
        }
        i += 1;
    }
    None
}

/// Project a lookup result onto the gap-fill candidate fields.
fn gap_values_from(meta: BookMetadata) -> GapValues {
    GapValues {
        summary: meta.summary,
        publisher: meta.publisher,
        page_count: meta.page_count.and_then(|p| i32::try_from(p).ok()),
        publication_year: meta.publication_year.as_deref().and_then(parse_year),
        cover_url: meta.cover_url,
    }
}

/// Adaptive politeness delay: base + jitter, widening once consecutive failures
/// exceed [`BACKOFF_AFTER`].
fn compute_delay_ms(consecutive_fail: u32) -> u64 {
    use rand::Rng;
    let jitter = rand::thread_rng().gen_range(0..200);
    let base = if consecutive_fail <= BACKOFF_AFTER {
        BASE_DELAY_MS
    } else {
        let shift = (consecutive_fail - BACKOFF_AFTER).min(16);
        BASE_DELAY_MS
            .saturating_mul(1u64 << shift)
            .min(MAX_DELAY_MS)
    };
    base + jitter
}

// ── Public API (called from FFI handlers and HTTP handlers) ───────────────

/// Library completeness statistic over owned books.
pub async fn stats(state: &AppState) -> Result<CompletenessStats, String> {
    state
        .metadata_fill_repo
        .completeness_stats()
        .await
        .map_err(err_to_string)
}

/// Owned, incomplete books with no ISBN (not processable; listed for manual fix).
pub async fn books_without_isbn(state: &AppState) -> Result<Vec<IncompleteBook>, String> {
    state
        .metadata_fill_repo
        .list_incomplete_without_isbn()
        .await
        .map_err(err_to_string)
}

/// Default cap for the manual "books to complete" overview.
const INCOMPLETE_LIST_LIMIT: u64 = 300;

/// All owned, incomplete books with their missing fields, closest-to-complete
/// first, for the manual completion overview.
pub async fn incomplete_books(
    state: &AppState,
    limit: Option<u64>,
) -> Result<Vec<IncompleteBookDetail>, String> {
    state
        .metadata_fill_repo
        .list_incomplete(limit.unwrap_or(INCOMPLETE_LIST_LIMIT))
        .await
        .map_err(err_to_string)
}

/// Current/last run state of any status (drives the live progress, the resume
/// offer, and the post-run summary). A `running` row with no live in-memory job
/// is reported (and persisted) as `interrupted` so the UI can offer to resume.
pub async fn progress(state: &AppState) -> Result<Option<FillRun>, String> {
    let repo = &state.metadata_fill_repo;
    let mut run = repo.last_run().await.map_err(err_to_string)?;
    if let Some(r) = &mut run
        && r.status == "running"
        && !state.metadata_fill.is_running()
    {
        r.status = "interrupted".to_string();
        let _ = repo.set_run_status(&r.batch_id, "interrupted").await;
    }
    Ok(run)
}

/// Recently completed books with the still-active fields this feature added.
pub async fn recent(state: &AppState, limit: u64) -> Result<Vec<RecentFilledBook>, String> {
    state
        .metadata_fill_repo
        .recent_filled(limit)
        .await
        .map_err(err_to_string)
}

/// Start (or resume) a bulk fill run. Idempotent while a run is live: returns
/// the in-flight batch id instead of starting a second run. `languages` is the
/// user's reading-language config (comma-joined), forwarded to the lookup for
/// summary-language coherence (ADR-040).
pub async fn start(state: &AppState, languages: Option<String>) -> Result<String, String> {
    let repo = state.metadata_fill_repo.clone();
    let manager = state.metadata_fill.clone();

    if let Some(active) = manager.active_batch() {
        return Ok(active);
    }

    // Resume an interrupted/leftover run, else open a fresh one.
    let (batch_id, start_cursor) = match repo.get_active_run().await.map_err(err_to_string)? {
        Some(run) => {
            repo.set_run_status(&run.batch_id, "running")
                .await
                .map_err(err_to_string)?;
            (run.batch_id, run.cursor_book_id)
        }
        None => {
            let total = repo
                .count_incomplete_with_isbn()
                .await
                .map_err(err_to_string)?;
            let batch_id = uuid::Uuid::new_v4().to_string();
            repo.create_run(&batch_id, total)
                .await
                .map_err(err_to_string)?;
            (batch_id, 0)
        }
    };

    let cancel = match manager.try_begin(batch_id.clone()) {
        Some(flag) => flag,
        // Lost the race to another caller — return whatever is now active.
        None => return Ok(manager.active_batch().unwrap_or(batch_id)),
    };

    let db = state.db().clone();
    let batch_for_task = batch_id.clone();
    tokio::spawn(async move {
        run_fill_loop(
            db,
            repo,
            manager,
            batch_for_task,
            languages,
            cancel,
            start_cursor,
        )
        .await;
    });

    Ok(batch_id)
}

/// Request cancellation. If a run is live the loop stops and marks itself
/// `cancelled`; if only a stale/interrupted run exists, it is marked `cancelled`
/// so it is no longer offered for resume.
pub async fn cancel(state: &AppState) -> Result<(), String> {
    state.metadata_fill.request_cancel();
    if !state.metadata_fill.is_running()
        && let Some(run) = state
            .metadata_fill_repo
            .get_active_run()
            .await
            .map_err(err_to_string)?
    {
        state
            .metadata_fill_repo
            .set_run_status(&run.batch_id, "cancelled")
            .await
            .map_err(err_to_string)?;
    }
    Ok(())
}

pub async fn undo_field(state: &AppState, journal_id: i64) -> Result<UndoOutcome, String> {
    state
        .metadata_fill_repo
        .undo_field(journal_id)
        .await
        .map_err(err_to_string)
}

pub async fn undo_book(state: &AppState, batch_id: &str, book_id: i32) -> Result<usize, String> {
    state
        .metadata_fill_repo
        .undo_book(batch_id, book_id)
        .await
        .map_err(err_to_string)
}

pub async fn undo_run(state: &AppState, batch_id: &str) -> Result<usize, String> {
    state
        .metadata_fill_repo
        .undo_run(batch_id)
        .await
        .map_err(err_to_string)
}

// ── Background loop ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_fill_loop(
    db: DatabaseConnection,
    repo: Arc<dyn MetadataFillRepository>,
    manager: Arc<MetadataFillManager>,
    batch_id: String,
    languages: Option<String>,
    cancel: Arc<AtomicBool>,
    start_cursor: i32,
) {
    // Reload counters so a resumed run keeps accumulating rather than resetting.
    let mut run = match repo.get_run(&batch_id).await {
        Ok(Some(r)) => r,
        _ => {
            manager.finish();
            return;
        }
    };
    let mut cursor = start_cursor;
    let mut consecutive_fail: u32 = 0;

    'outer: loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = repo.set_run_status(&batch_id, "cancelled").await;
            break;
        }
        let batch = match repo.list_incomplete_with_isbn(cursor, PAGE).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("metadata_fill: work-list query failed: {e}");
                let _ = repo.set_run_status(&batch_id, "interrupted").await;
                break;
            }
        };
        if batch.is_empty() {
            let _ = repo.set_run_status(&batch_id, "done").await;
            break;
        }

        for book in batch {
            if cancel.load(Ordering::SeqCst) {
                let _ = repo.set_run_status(&batch_id, "cancelled").await;
                break 'outer;
            }
            let Some(isbn) = book.isbn.clone().filter(|s| !s.trim().is_empty()) else {
                cursor = book.id;
                continue;
            };

            run.current_title = Some(book.title.clone());
            let _ = repo.update_run_progress(&run).await;

            tokio::time::sleep(Duration::from_millis(compute_delay_ms(consecutive_fail))).await;

            match crate::services::lookup_service::lookup_metadata_by_isbn(
                &db,
                &isbn,
                languages.as_deref(),
            )
            .await
            {
                Ok(Some(meta)) => {
                    consecutive_fail = 0;
                    match repo
                        .apply_fill(&batch_id, book.id, gap_values_from(meta))
                        .await
                    {
                        Ok(filled) if !filled.is_empty() => run.filled += 1,
                        Ok(_) => run.skipped += 1,
                        Err(e) => {
                            run.errored += 1;
                            tracing::warn!("metadata_fill: apply_fill failed for {}: {e}", book.id);
                        }
                    }
                }
                Ok(None) => {
                    run.skipped += 1;
                    consecutive_fail = consecutive_fail.saturating_add(1);
                }
                Err(e) => {
                    run.errored += 1;
                    consecutive_fail = consecutive_fail.saturating_add(1);
                    tracing::debug!("metadata_fill: lookup failed for {isbn}: {e}");
                }
            }

            run.done += 1;
            cursor = book.id;
            run.cursor_book_id = cursor;
            let _ = repo.update_run_progress(&run).await;
        }
    }

    manager.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_year_extracts_four_digits() {
        assert_eq!(parse_year("1998"), Some(1998));
        assert_eq!(parse_year("1998-03-01"), Some(1998));
        assert_eq!(parse_year("publié en 2001."), Some(2001));
        assert_eq!(parse_year("n/a"), None);
        assert_eq!(parse_year(""), None);
    }

    #[test]
    fn gap_values_converts_types() {
        let meta = BookMetadata {
            title: "T".into(),
            authors: vec![],
            publisher: Some("P".into()),
            publication_year: Some("2010-01".into()),
            cover_url: None,
            summary: Some("S".into()),
            page_count: Some(250),
        };
        let g = gap_values_from(meta);
        assert_eq!(g.publisher.as_deref(), Some("P"));
        assert_eq!(g.publication_year, Some(2010));
        assert_eq!(g.page_count, Some(250));
        assert_eq!(g.summary.as_deref(), Some("S"));
        assert_eq!(g.cover_url, None);
    }

    #[test]
    fn delay_backs_off_after_threshold() {
        // Below/at threshold stays near base; well beyond it grows.
        assert!(compute_delay_ms(0) >= BASE_DELAY_MS);
        assert!(compute_delay_ms(BACKOFF_AFTER) < BASE_DELAY_MS * 2 + 200);
        assert!(compute_delay_ms(BACKOFF_AFTER + 3) > BASE_DELAY_MS * 2);
        assert!(compute_delay_ms(50) <= MAX_DELAY_MS + 200);
    }

    #[test]
    fn manager_enforces_single_run() {
        let m = MetadataFillManager::new();
        assert!(!m.is_running());
        let flag = m.try_begin("b1".into()).expect("first begin succeeds");
        assert!(m.is_running());
        assert_eq!(m.active_batch().as_deref(), Some("b1"));
        // second concurrent begin is refused
        assert!(m.try_begin("b2".into()).is_none());
        // cancel flips the flag
        m.request_cancel();
        assert!(flag.load(Ordering::SeqCst));
        m.finish();
        assert!(!m.is_running());
    }
}
