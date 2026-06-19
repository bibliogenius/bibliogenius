//! HTTP API for the bulk metadata gap-fill feature (ADR-041).
//!
//! Thin Axum handlers that delegate to `services::metadata_fill_service`. The
//! same service backs the FFI surface in `api/frb.rs` (Rule F3: both channels).

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::json;

use crate::domain::metadata_fill::{FillRun, RecentFilledBook, UndoOutcome};
use crate::infrastructure::AppState;
use crate::services::metadata_fill_service as svc;

fn err(e: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e })),
    )
}

pub async fn get_stats(State(state): State<AppState>) -> impl IntoResponse {
    match svc::stats(&state).await {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({
                "owned_total": s.owned_total,
                "complete": s.complete,
                "incomplete": s.incomplete,
                "no_isbn": s.no_isbn,
                "empty_fields": s.empty_fields,
            })),
        )
            .into_response(),
        Err(e) => err(e).into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct StartBody {
    /// Comma-joined reading languages for summary coherence (ADR-040).
    pub languages: Option<String>,
    /// Per-invocation lot quota; `None` runs the whole backlog (ADR-041).
    pub lot_limit: Option<u64>,
}

pub async fn start(
    State(state): State<AppState>,
    body: Option<Json<StartBody>>,
) -> impl IntoResponse {
    let (languages, lot_limit) = body
        .map(|b| (b.0.languages, b.0.lot_limit))
        .unwrap_or((None, None));
    match svc::start(&state, languages, lot_limit).await {
        Ok(batch_id) => (StatusCode::OK, Json(json!({ "batch_id": batch_id }))).into_response(),
        Err(e) => err(e).into_response(),
    }
}

fn run_json(run: &FillRun) -> serde_json::Value {
    json!({
        "batch_id": run.batch_id,
        "status": run.status,
        "total": run.total,
        "done": run.done,
        "filled": run.filled,
        "skipped": run.skipped,
        "errored": run.errored,
        "current_title": run.current_title,
    })
}

pub async fn get_progress(State(state): State<AppState>) -> impl IntoResponse {
    match svc::progress(&state).await {
        Ok(Some(run)) => (StatusCode::OK, Json(run_json(&run))).into_response(),
        Ok(None) => (StatusCode::OK, Json(serde_json::Value::Null)).into_response(),
        Err(e) => err(e).into_response(),
    }
}

pub async fn cancel(State(state): State<AppState>) -> impl IntoResponse {
    match svc::cancel(&state).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct RecentQuery {
    pub limit: Option<u64>,
}

fn recent_json(books: &[RecentFilledBook]) -> serde_json::Value {
    json!(
        books
            .iter()
            .map(|b| json!({
                "book_id": b.book_id,
                "title": b.title,
                "cover_url": b.cover_url,
                "fields": b.fields.iter().map(|f| json!({
                    "journal_id": f.journal_id,
                    "batch_id": f.batch_id,
                    "field": f.field,
                    "value": f.value,
                })).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>()
    )
}

pub async fn get_recent(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<RecentQuery>,
) -> impl IntoResponse {
    match svc::recent(&state, q.limit.unwrap_or(50)).await {
        Ok(books) => (StatusCode::OK, Json(recent_json(&books))).into_response(),
        Err(e) => err(e).into_response(),
    }
}

pub async fn get_no_isbn(State(state): State<AppState>) -> impl IntoResponse {
    match svc::books_without_isbn(&state).await {
        Ok(books) => (
            StatusCode::OK,
            Json(json!(
                books
                    .iter()
                    .map(|b| json!({ "id": b.id, "title": b.title, "isbn": b.isbn }))
                    .collect::<Vec<_>>()
            )),
        )
            .into_response(),
        Err(e) => err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct IncompleteQuery {
    pub limit: Option<u64>,
}

pub async fn get_incomplete(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<IncompleteQuery>,
) -> impl IntoResponse {
    match svc::incomplete_books(&state, q.limit).await {
        Ok(books) => (
            StatusCode::OK,
            Json(json!(
                books
                    .iter()
                    .map(|b| json!({
                        "id": b.id,
                        "title": b.title,
                        "isbn": b.isbn,
                        "cover_url": b.cover_url,
                        "missing": b.missing,
                    }))
                    .collect::<Vec<_>>()
            )),
        )
            .into_response(),
        Err(e) => err(e).into_response(),
    }
}

fn outcome_str(o: UndoOutcome) -> &'static str {
    match o {
        UndoOutcome::Reverted => "reverted",
        UndoOutcome::Superseded => "superseded",
        UndoOutcome::NotFound => "not_found",
    }
}

pub async fn undo_field(
    State(state): State<AppState>,
    Path(journal_id): Path<i64>,
) -> impl IntoResponse {
    match svc::undo_field(&state, journal_id).await {
        Ok(o) => (StatusCode::OK, Json(json!({ "outcome": outcome_str(o) }))).into_response(),
        Err(e) => err(e).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct UndoBookBody {
    pub batch_id: String,
    pub book_id: i32,
}

pub async fn undo_book(
    State(state): State<AppState>,
    Json(body): Json<UndoBookBody>,
) -> impl IntoResponse {
    match svc::undo_book(&state, &body.batch_id, body.book_id).await {
        Ok(n) => (StatusCode::OK, Json(json!({ "reverted": n }))).into_response(),
        Err(e) => err(e).into_response(),
    }
}

pub async fn undo_run(
    State(state): State<AppState>,
    Path(batch_id): Path<String>,
) -> impl IntoResponse {
    match svc::undo_run(&state, &batch_id).await {
        Ok(n) => (StatusCode::OK, Json(json!({ "reverted": n }))).into_response(),
        Err(e) => err(e).into_response(),
    }
}
