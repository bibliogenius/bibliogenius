use crate::models::copy::{self as copy_model, Entity as Copy};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
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
    pub book_title: Option<String>,
    pub price: Option<f64>,
    pub sold_at: Option<String>,
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
            book_title: None,
            price: model.price,
            sold_at: model.sold_at,
        }
    }
}

// List all copies with book details
pub async fn list_copies(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::book::Entity as Book;

    match Copy::find().find_also_related(Book).all(&db).await {
        Ok(copies_with_books) => {
            let copy_dtos: Vec<CopyDto> = copies_with_books
                .into_iter()
                .map(|(copy, book)| {
                    let mut dto = CopyDto::from(copy);
                    dto.book_title = book.map(|b| b.title);
                    dto
                })
                .collect();

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
        price: Set(copy_dto.price),
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

/// Get borrowed copies (is_temporary=true) with book details
pub async fn get_borrowed_copies(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::book::Entity as Book;

    match Copy::find()
        .filter(copy_model::Column::IsTemporary.eq(true))
        .find_also_related(Book)
        .all(&db)
        .await
    {
        Ok(copies_with_books) => {
            let borrowed: Vec<serde_json::Value> = copies_with_books
                .into_iter()
                .map(|(copy, book)| {
                    serde_json::json!({
                        "id": copy.id,
                        "book_id": copy.book_id,
                        "title": book.as_ref().map(|b| b.title.clone()).unwrap_or_default(),
                        "cover": book.as_ref().and_then(|b| b.cover_url.clone()),
                        "status": copy.status,
                        "notes": copy.notes,
                        "acquisition_date": copy.acquisition_date,
                        "from_contact": copy.notes.clone() // Notes contains "Borrowed from: Name (ID: x)"
                    })
                })
                .collect();

            Json(serde_json::json!({
                "loans": borrowed,
                "total": borrowed.len()
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

/// DTO for partial copy updates
#[derive(Debug, Deserialize)]
pub struct UpdateCopyDto {
    pub status: Option<String>,
    pub notes: Option<Option<String>>,
    pub acquisition_date: Option<Option<String>>,
    pub price: Option<Option<f64>>,
}

/// Update a copy (mainly for status changes)
pub async fn update_copy(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
    Json(payload): Json<UpdateCopyDto>,
) -> impl IntoResponse {
    // Find existing copy
    let copy = match Copy::find_by_id(id).one(&db).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Copy not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Database error: {}", e)})),
            )
                .into_response();
        }
    };

    // Update fields
    let mut active: copy_model::ActiveModel = copy.into();
    if let Some(status) = payload.status {
        active.status = Set(status);
    }

    if let Some(notes) = payload.notes {
        active.notes = Set(notes);
    }

    if let Some(date) = payload.acquisition_date {
        active.acquisition_date = Set(date);
    }

    if let Some(price) = payload.price {
        active.price = Set(price);
    }
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(model) => {
            let dto = CopyDto::from(model);
            (StatusCode::OK, Json(serde_json::json!({"copy": dto}))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to update copy: {}", e)})),
        )
            .into_response(),
    }
}
