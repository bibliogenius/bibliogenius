// Reimport to complete: filling an ISBN-less library from its source file
// (ADR-071). Included by api/frb.rs (include!, not a module): items must stay
// in crate::api::frb so the generated bindings keep their names, and this file
// is last in the include! order because it is the newest concern. Shared
// imports live in frb.rs.

/// One parsed row of the reimported file. Dart owns the parsing (the CSV/XLSX
/// readers already exist there); this carries the result across.
#[frb(dart_metadata=("freezed"))]
pub struct FrbImportRow {
    pub title: String,
    /// The author cell exactly as the file gives it, undivided.
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
}

/// A row the campaign did not use, for the two consultable lists.
#[frb(dart_metadata=("freezed"))]
pub struct FrbSkippedImportRow {
    pub title: String,
    pub author: Option<String>,
    /// `no_match` | `ambiguous_in_file` | `ambiguous_in_library`.
    pub reason: String,
}

/// Outcome of one "reimport to complete" campaign.
#[frb(dart_metadata=("freezed"))]
pub struct FrbImportCompletionReport {
    /// Undo handle: `metadata_fill_undo_run(batch_id)` reverts the campaign.
    pub batch_id: String,
    pub rows_read: i64,
    pub completed: i64,
    pub fields_written: i64,
    pub no_match: i64,
    pub ambiguous: i64,
    /// Bounded sample of the skipped rows; the counters above are exact.
    pub skipped: Vec<FrbSkippedImportRow>,
}

/// The largest same-day group of owned books with no ISBN.
#[frb(dart_metadata=("freezed"))]
pub struct FrbNoIsbnCluster {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub count: i64,
}

/// Match every row against the library and fill what the matched books are
/// missing (`isbn`, `publisher`, `publication_year`), `None`-only and
/// journalled under one batch id. Creates nothing and overwrites nothing.
pub async fn import_complete_from_rows(
    rows: Vec<FrbImportRow>,
) -> Result<FrbImportCompletionReport, String> {
    use crate::services::import_completion_service as svc;

    let state = global_app_state().ok_or_else(|| "AppState not initialized".to_string())?;
    let rows: Vec<svc::ImportRow> = rows
        .into_iter()
        .map(|r| svc::ImportRow {
            title: r.title,
            author: r.author,
            isbn: r.isbn,
            publisher: r.publisher,
            publication_year: r.publication_year,
        })
        .collect();

    let report = svc::complete_from_rows(state, rows)
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbImportCompletionReport {
        batch_id: report.batch_id,
        rows_read: report.rows_read,
        completed: report.completed,
        fields_written: report.fields_written,
        no_match: report.no_match,
        ambiguous: report.ambiguous,
        skipped: report
            .skipped
            .into_iter()
            .map(|r| FrbSkippedImportRow {
                title: r.title,
                author: r.author,
                reason: r.reason.as_str().to_string(),
            })
            .collect(),
    })
}

/// Second signal behind the "your import lost its ISBNs" banner: the biggest
/// group of ISBN-less owned books added on a single day.
pub async fn import_no_isbn_cluster() -> Result<Option<FrbNoIsbnCluster>, String> {
    let db = db().ok_or("Database not initialized")?;
    let cluster = crate::services::import_completion_service::no_isbn_cluster(db)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cluster.map(|c| FrbNoIsbnCluster {
        day: c.day,
        count: c.count,
    }))
}
