use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::domain::DomainError;
use crate::infrastructure::AppState;

#[derive(Deserialize)]
pub struct CreateAuthorRequest {
    name: String,
}

pub async fn list_authors(State(state): State<AppState>) -> impl IntoResponse {
    match state.author_repo.find_all().await {
        Ok(authors) => (StatusCode::OK, Json(authors)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn create_author(
    State(state): State<AppState>,
    Json(payload): Json<CreateAuthorRequest>,
) -> impl IntoResponse {
    match state.author_repo.create(payload.name).await {
        Ok(author) => (StatusCode::CREATED, Json(author)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn get_author(State(state): State<AppState>, Path(id): Path<i32>) -> impl IntoResponse {
    match state.author_repo.find_by_id(id).await {
        Ok(Some(author)) => (StatusCode::OK, Json(author)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Author not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_author(
    State(state): State<AppState>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    match state.author_repo.delete(id).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "message": "Author deleted" }))).into_response(),
        Err(DomainError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Author not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
