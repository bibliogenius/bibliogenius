//! Axum handlers for the operation log viewer

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::infrastructure::AppState;

use super::domain::{OperationLogFilter, OperationLogViewerRepository};
use super::repository::SeaOrmOperationLogViewerRepository;

#[derive(Deserialize, Default)]
pub struct LogQuery {
    pub entity_type: Option<String>,
    pub operation: Option<String>,
    pub status: Option<String>,
    pub q: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub page: Option<u64>,
    pub limit: Option<u64>,
}

/// GET /api/admin/operation-log
pub async fn list_entries(
    State(state): State<AppState>,
    Query(q): Query<LogQuery>,
) -> impl IntoResponse {
    let repo = SeaOrmOperationLogViewerRepository::new(state.db());
    let filter = OperationLogFilter {
        entity_type: q.entity_type,
        operation: q.operation,
        status: q.status,
        query: q.q,
        since: q.since,
        until: q.until,
        page: q.page.unwrap_or(0),
        limit: q.limit.unwrap_or(50).min(200),
    };

    match repo.find_all(filter).await {
        Ok(page) => (StatusCode::OK, Json(json!(page))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/admin/operation-log/stats
pub async fn get_stats(State(state): State<AppState>) -> impl IntoResponse {
    let repo = SeaOrmOperationLogViewerRepository::new(state.db());

    match repo.get_stats().await {
        Ok(stats) => (StatusCode::OK, Json(json!(stats))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /api/admin/operation-log/entity-types
pub async fn get_entity_types(State(state): State<AppState>) -> impl IntoResponse {
    let repo = SeaOrmOperationLogViewerRepository::new(state.db());

    match repo.get_entity_types().await {
        Ok(types) => (StatusCode::OK, Json(json!(types))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
