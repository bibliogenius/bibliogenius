//! Copy API handlers using repository pattern

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::domain::{CreateCopyInput, DomainError, UpdateCopyInput};
use crate::infrastructure::AppState;

// List all copies with book details
pub async fn list_copies(State(state): State<AppState>) -> impl IntoResponse {
    match state.copy_repo.find_all().await {
        Ok(result) => Json(json!({
            "copies": result.copies,
            "total": result.total
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

/// Request DTO for creating a copy
#[derive(Debug, Deserialize)]
pub struct CreateCopyRequest {
    pub book_id: i32,
    pub library_id: i32,
    pub acquisition_date: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    pub is_temporary: bool,
    pub price: Option<f64>,
}

// Create a new copy
pub async fn create_copy(
    State(state): State<AppState>,
    Json(payload): Json<CreateCopyRequest>,
) -> impl IntoResponse {
    let input = CreateCopyInput {
        book_id: payload.book_id,
        library_id: payload.library_id,
        acquisition_date: payload.acquisition_date,
        notes: payload.notes,
        status: payload.status,
        is_temporary: payload.is_temporary,
        price: payload.price,
    };

    match state.copy_repo.create(input).await {
        Ok(copy) => (
            StatusCode::CREATED,
            Json(json!({
                "copy": copy,
                "message": "Copy created successfully"
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to create copy: {}", e)})),
        )
            .into_response(),
    }
}

// Get a single copy by ID
pub async fn get_copy(State(state): State<AppState>, Path(id): Path<i32>) -> impl IntoResponse {
    match state.copy_repo.find_by_id(id).await {
        Ok(Some(copy)) => (StatusCode::OK, Json(json!({"copy": copy}))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Copy not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

// Get copies of a specific book
pub async fn get_book_copies(
    State(state): State<AppState>,
    Path(book_id): Path<i32>,
) -> impl IntoResponse {
    match state.copy_repo.find_by_book_id(book_id).await {
        Ok(result) => Json(json!({
            "copies": result.copies,
            "total": result.total
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

/// Get borrowed copies (is_temporary=true) with book details
/// Returns "loans" key for Flutter compatibility
pub async fn get_borrowed_copies(State(state): State<AppState>) -> impl IntoResponse {
    match state.copy_repo.find_borrowed().await {
        Ok(result) => {
            // Transform to "loans" format for Flutter compatibility
            let loans: Vec<serde_json::Value> = result
                .copies
                .into_iter()
                .map(|copy| {
                    json!({
                        "id": copy.id,
                        "book_id": copy.book_id,
                        "title": copy.book_title.unwrap_or_default(),
                        "cover": copy.book_cover,
                        "status": copy.status,
                        "notes": copy.notes,
                        "acquisition_date": copy.acquisition_date,
                        "from_contact": copy.notes  // Notes contains "Borrowed from: Name (ID: x)"
                    })
                })
                .collect();

            let total = loans.len();
            Json(json!({
                "loans": loans,
                "total": total
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

// Delete a copy
pub async fn delete_copy(State(state): State<AppState>, Path(id): Path<i32>) -> impl IntoResponse {
    match state.copy_repo.delete(id).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"message": "Copy deleted successfully"})),
        )
            .into_response(),
        Err(DomainError::NotFound) => {
            // Idempotent delete - return OK even if not found
            (
                StatusCode::OK,
                Json(json!({"message": "Copy deleted successfully"})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to delete copy: {}", e)})),
        )
            .into_response(),
    }
}

/// DTO for partial copy updates
#[derive(Debug, Deserialize)]
pub struct UpdateCopyRequest {
    pub status: Option<String>,
    pub notes: Option<Option<String>>,
    pub acquisition_date: Option<Option<String>>,
    pub price: Option<Option<f64>>,
}

/// Update a copy (mainly for status changes)
pub async fn update_copy(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    Json(payload): Json<UpdateCopyRequest>,
) -> impl IntoResponse {
    let input = UpdateCopyInput {
        status: payload.status,
        notes: payload.notes,
        acquisition_date: payload.acquisition_date,
        price: payload.price,
    };

    match state.copy_repo.update(id, input).await {
        Ok(copy) => (StatusCode::OK, Json(json!({"copy": copy}))).into_response(),
        Err(DomainError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Copy not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Failed to update copy: {}", e)})),
        )
            .into_response(),
    }
}
