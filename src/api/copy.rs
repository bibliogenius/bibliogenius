use crate::models::copy::{self as copy_model, Entity as Copy};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct CopyDto {
    pub id: Option<i32>,
    pub book_id: i32,
    pub library_id: i32,
    pub acquisition_date: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    pub is_temporary: bool,
}

impl From<copy_model::Model> for CopyDto {
    fn from(model: copy_model::Model) -> Self {
        Self {
            id: Some(model.id),
            book_id: model.book_id,
            library_id: model.library_id,
            acquisition_date: model.acquisition_date,
            notes: model.notes,
            status: model.status,
            is_temporary: model.is_temporary,
        }
    }
}

// List all copies
pub async fn list_copies(
    State(db): State<DatabaseConnection>,
) -> impl IntoResponse {
    match Copy::find().all(&db).await {
        Ok(copies) => {
            let copy_dtos: Vec<CopyDto> = copies.into_iter().map(CopyDto::from).collect();
            Json(serde_json::json!({
                "copies": copy_dtos,
                "total": copy_dtos.len()
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

// Create a new copy
pub async fn create_copy(
    State(db): State<DatabaseConnection>,
    Json(copy_dto): Json<CopyDto>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().to_rfc3339();

    let new_copy = copy_model::ActiveModel {
        book_id: Set(copy_dto.book_id),
        library_id: Set(copy_dto.library_id),
        acquisition_date: Set(copy_dto.acquisition_date),
        notes: Set(copy_dto.notes),
        status: Set(copy_dto.status),
        is_temporary: Set(copy_dto.is_temporary),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(&db).await {
        Ok(model) => {
            let copy_dto = CopyDto::from(model);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "copy": copy_dto,
                    "message": "Copy created successfully"
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to create copy: {}", e)})),
        )
            .into_response(),
    }
}

// Get copies of a specific book
pub async fn get_book_copies(
    State(db): State<DatabaseConnection>,
    Path(book_id): Path<i32>,
) -> impl IntoResponse {
    match Copy::find()
        .filter(copy_model::Column::BookId.eq(book_id))
        .all(&db)
        .await
    {
        Ok(copies) => {
            let copy_dtos: Vec<CopyDto> = copies.into_iter().map(CopyDto::from).collect();
            Json(serde_json::json!({
                "copies": copy_dtos,
                "total": copy_dtos.len()
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

// Delete a copy
pub async fn delete_copy(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    match Copy::delete_by_id(id).exec(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"message": "Copy deleted successfully"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to delete copy: {}", e)})),
        )
            .into_response(),
    }
}
