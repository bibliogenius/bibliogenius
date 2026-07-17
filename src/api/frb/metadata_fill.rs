// Metadata gap-fill: stats, runs, progress, undo (ADR-040).
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

/// Metadata fetched from external sources for a book refresh.
/// Each field is optional - only non-null fields have data from the source.
#[frb(dart_metadata=("freezed"))]
pub struct FrbBookMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
    pub summary: Option<String>,
    pub page_count: Option<u32>,
}

/// Look up book metadata by ISBN from external sources (BNF, Inventaire, OpenLibrary, etc.).
/// Used by the metadata refresh feature to let users preview and cherry-pick fields.
pub async fn lookup_book_metadata(
    isbn: String,
    lang: Option<String>,
) -> Result<Option<FrbBookMetadata>, String> {
    let db = db().ok_or("Database not initialized")?;
    let result =
        crate::services::lookup_service::lookup_metadata_by_isbn(db, &isbn, lang.as_deref())
            .await?;
    Ok(result.map(|m| FrbBookMetadata {
        title: Some(m.title),
        author: if m.authors.is_empty() {
            None
        } else {
            Some(
                m.authors
                    .iter()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        },
        publisher: m.publisher,
        publication_year: m.publication_year,
        cover_url: m.cover_url,
        summary: m.summary,
        page_count: m.page_count,
    }))
}

// ============ Bulk metadata gap-fill (ADR-041) ============

/// Library completeness snapshot over owned books.
#[frb(dart_metadata=("freezed"))]
pub struct FrbCompletenessStats {
    pub owned_total: i64,
    pub complete: i64,
    pub incomplete: i64,
    pub no_isbn: i64,
    pub empty_fields: i64,
}

/// Live/last progress of a bulk fill run.
#[frb(dart_metadata=("freezed"))]
pub struct FrbFillProgress {
    pub batch_id: String,
    /// `running` | `done` | `cancelled` | `interrupted`.
    pub status: String,
    pub total: i64,
    pub done: i64,
    pub filled: i64,
    pub skipped: i64,
    pub errored: i64,
    pub current_title: Option<String>,
}

/// One field added to a book by the bulk fill, for the undo list.
#[frb(dart_metadata=("freezed"))]
pub struct FrbFilledField {
    pub journal_id: i64,
    pub batch_id: String,
    pub field: String,
    pub value: String,
}

/// A recently-completed book with the fields the fill added to it.
#[frb(dart_metadata=("freezed"))]
pub struct FrbFilledBook {
    pub book_id: String,
    pub title: String,
    pub cover_url: Option<String>,
    pub fields: Vec<FrbFilledField>,
}

/// A book that could not be processed (no ISBN), for the manual-fix list.
#[frb(dart_metadata=("freezed"))]
pub struct FrbIncompleteBook {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
}

/// An incomplete owned book with the exact fields still empty, for the manual
/// "books to complete" overview.
#[frb(dart_metadata=("freezed"))]
pub struct FrbIncompleteBookDetail {
    pub id: String,
    pub title: String,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub missing: Vec<String>,
}

fn fill_state() -> Result<&'static crate::infrastructure::AppState, String> {
    global_app_state().ok_or_else(|| "AppState not initialized".to_string())
}

/// Completeness statistic for the dashboard "Complétude" card.
pub async fn metadata_fill_stats() -> Result<FrbCompletenessStats, String> {
    let s = crate::services::metadata_fill_service::stats(fill_state()?).await?;
    Ok(FrbCompletenessStats {
        owned_total: s.owned_total,
        complete: s.complete,
        incomplete: s.incomplete,
        no_isbn: s.no_isbn,
        empty_fields: s.empty_fields,
    })
}

/// Start (or resume) the bulk fill. Returns the batch id. `languages` is the
/// user's reading-language config (comma-joined) for summary coherence.
/// `lot_limit` caps how many books this invocation processes before pausing the
/// run as resumable (the "small batches" nudge); `None` runs to completion.
pub async fn metadata_fill_start(
    languages: Option<String>,
    lot_limit: Option<u32>,
) -> Result<String, String> {
    crate::services::metadata_fill_service::start(
        fill_state()?,
        languages,
        lot_limit.map(|l| l as u64),
    )
    .await
}

/// Current/last run progress (None if a run has never been started).
pub async fn metadata_fill_progress() -> Result<Option<FrbFillProgress>, String> {
    let run = crate::services::metadata_fill_service::progress(fill_state()?).await?;
    Ok(run.map(|r| FrbFillProgress {
        batch_id: r.batch_id,
        status: r.status,
        total: r.total,
        done: r.done,
        filled: r.filled,
        skipped: r.skipped,
        errored: r.errored,
        current_title: r.current_title,
    }))
}

/// Request cancellation of the active run.
pub async fn metadata_fill_cancel() -> Result<(), String> {
    crate::services::metadata_fill_service::cancel(fill_state()?).await
}

/// Recently completed books with the still-active fields the fill added.
pub async fn metadata_fill_recent(limit: u32) -> Result<Vec<FrbFilledBook>, String> {
    let books = crate::services::metadata_fill_service::recent(fill_state()?, limit as u64).await?;
    Ok(books
        .into_iter()
        .map(|b| FrbFilledBook {
            // The book id is now a uuid String; FrbFilledBook.book_id is still an
            // i32 (its codec is frozen in frb_generated.rs). Bridge here until the
            // FFI struct is migrated to carry the uuid.
            book_id: b.book_id,
            title: b.title,
            cover_url: b.cover_url,
            fields: b
                .fields
                .into_iter()
                .map(|f| FrbFilledField {
                    journal_id: f.journal_id,
                    batch_id: f.batch_id,
                    field: f.field,
                    value: f.value,
                })
                .collect(),
        })
        .collect())
}

/// Owned, incomplete books without an ISBN (not processable; manual fix list).
pub async fn metadata_fill_books_without_isbn() -> Result<Vec<FrbIncompleteBook>, String> {
    let books = crate::services::metadata_fill_service::books_without_isbn(fill_state()?).await?;
    Ok(books
        .into_iter()
        .map(|b| FrbIncompleteBook {
            // uuid String -> frozen i32 FFI field; bridge until the struct migrates.
            id: b.id,
            title: b.title,
            isbn: b.isbn,
        })
        .collect())
}

/// All owned, incomplete books with their missing fields (closest-to-complete
/// first), for the manual completion overview.
pub async fn metadata_fill_incomplete(
    limit: Option<u32>,
) -> Result<Vec<FrbIncompleteBookDetail>, String> {
    let books = crate::services::metadata_fill_service::incomplete_books(
        fill_state()?,
        limit.map(|l| l as u64),
    )
    .await?;
    Ok(books
        .into_iter()
        .map(|b| FrbIncompleteBookDetail {
            // uuid String -> frozen i32 FFI field; bridge until the struct migrates.
            id: b.id,
            title: b.title,
            isbn: b.isbn,
            cover_url: b.cover_url,
            missing: b.missing,
        })
        .collect())
}

/// Undo a single filled field. Returns `reverted` | `superseded` | `not_found`.
pub async fn metadata_fill_undo_field(journal_id: i64) -> Result<String, String> {
    let outcome =
        crate::services::metadata_fill_service::undo_field(fill_state()?, journal_id).await?;
    Ok(undo_outcome_str(outcome).to_string())
}

/// Undo all fields the fill added to one book in a batch. Returns reverted count.
pub async fn metadata_fill_undo_book(batch_id: String, book_id: String) -> Result<u32, String> {
    let n = crate::services::metadata_fill_service::undo_book(fill_state()?, &batch_id, &book_id)
        .await?;
    Ok(n as u32)
}

/// Undo every field a whole run added. Returns reverted count.
pub async fn metadata_fill_undo_run(batch_id: String) -> Result<u32, String> {
    let n = crate::services::metadata_fill_service::undo_run(fill_state()?, &batch_id).await?;
    Ok(n as u32)
}

fn undo_outcome_str(outcome: crate::domain::metadata_fill::UndoOutcome) -> &'static str {
    use crate::domain::metadata_fill::UndoOutcome;
    match outcome {
        UndoOutcome::Reverted => "reverted",
        UndoOutcome::Superseded => "superseded",
        UndoOutcome::NotFound => "not_found",
    }
}
