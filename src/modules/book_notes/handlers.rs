//! Axum handlers for book notes CRUD.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::json;

use super::domain::{
    BookNoteRepository, CreateBookNoteInput, MAX_CONTENT_LENGTH, UpdateBookNoteInput,
};
use super::repository::SeaOrmBookNoteRepository;
use crate::infrastructure::AppState;

fn repo(state: &AppState) -> SeaOrmBookNoteRepository {
    SeaOrmBookNoteRepository::new(state.db().clone())
}

/// GET /books/:id/notes
pub async fn list_notes(
    State(state): State<AppState>,
    Path(book_id): Path<i32>,
) -> impl IntoResponse {
    match repo(&state).find_by_book_id(book_id).await {
        Ok(notes) => (StatusCode::OK, Json(json!(notes))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /books/:id/notes
pub async fn create_note(
    State(state): State<AppState>,
    Path(book_id): Path<i32>,
    Json(input): Json<CreateBookNoteInput>,
) -> impl IntoResponse {
    if input.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Content cannot be empty"})),
        )
            .into_response();
    }
    if input.content.len() > MAX_CONTENT_LENGTH {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Content exceeds {} characters", MAX_CONTENT_LENGTH)})),
        )
            .into_response();
    }

    match repo(&state).create(book_id, input).await {
        Ok(note) => {
            // Payload needed for device sync (linked devices only, not P2P peers).
            let payload = json!({
                "book_id": note.book_id,
                "content": note.content,
                "page": note.page,
            });
            let _ = crate::sync::log_operation(
                state.db(),
                "book_note",
                note.id,
                "INSERT",
                Some(payload),
            )
            .await;
            (StatusCode::CREATED, Json(json!(note))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// PUT /book-notes/:id
pub async fn update_note(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    Json(input): Json<UpdateBookNoteInput>,
) -> impl IntoResponse {
    if input.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Content cannot be empty"})),
        )
            .into_response();
    }
    if input.content.len() > MAX_CONTENT_LENGTH {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Content exceeds {} characters", MAX_CONTENT_LENGTH)})),
        )
            .into_response();
    }

    match repo(&state).update(id, input).await {
        Ok(note) => {
            let payload = json!({
                "book_id": note.book_id,
                "content": note.content,
                "page": note.page,
            });
            let _ =
                crate::sync::log_operation(state.db(), "book_note", id, "UPDATE", Some(payload))
                    .await;
            (StatusCode::OK, Json(json!(note))).into_response()
        }
        Err(crate::domain::DomainError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Note not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// DELETE /book-notes/:id
pub async fn delete_note(State(state): State<AppState>, Path(id): Path<i32>) -> impl IntoResponse {
    match repo(&state).delete(id).await {
        Ok(()) => {
            let _ = crate::sync::log_operation(state.db(), "book_note", id, "DELETE", None).await;
            (
                StatusCode::OK,
                Json(json!({"message": "Note deleted successfully"})),
            )
                .into_response()
        }
        Err(crate::domain::DomainError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Note not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
