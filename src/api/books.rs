use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{json, Value};

use crate::models::book::{ActiveModel, Entity as BookEntity};
use crate::models::Book;

pub async fn list_books(State(db): State<DatabaseConnection>) -> Result<Json<Value>, StatusCode> {
    let books = BookEntity::find()
        .all(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let book_dtos: Vec<Book> = books.into_iter().map(Book::from).collect();

    Ok(Json(json!({
        "books": book_dtos,
        "total": book_dtos.len()
    })))
}

pub async fn create_book(
    State(db): State<DatabaseConnection>,
    Json(book): Json<Book>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    let subjects_json = book
        .subjects
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

    let new_book = ActiveModel {
        title: Set(book.title),
        isbn: Set(book.isbn),
        summary: Set(book.summary),
        publisher: Set(book.publisher),
        publication_year: Set(book.publication_year),
        dewey_decimal: Set(book.dewey_decimal),
        lcc: Set(book.lcc),
        subjects: Set(subjects_json),
        marc_record: Set(book.marc_record),
        cataloguing_notes: Set(book.cataloguing_notes),
        source_data: Set(book.source_data),
        created_at: Set(now.to_rfc3339()),
        updated_at: Set(now.to_rfc3339()),
        ..Default::default()
    };

    match new_book.insert(&db).await {
        Ok(model) => {
            // Log operation
            let _ = crate::sync::log_operation(
                &db,
                "book",
                model.id,
                "INSERT",
                Some(serde_json::to_value(&model).unwrap()),
            )
            .await;

            (
                StatusCode::CREATED,
                Json(json!({
                    "message": "Book created successfully",
                    "book": Book::from(model)
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_book(
    State(db): State<DatabaseConnection>,
    axum::extract::Path(id): axum::extract::Path<i32>,
) -> impl IntoResponse {
    match BookEntity::delete_by_id(id).exec(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({"message": "Book deleted successfully"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
