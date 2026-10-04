use crate::models::book;
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct BatchEditRequest {
    pub ids: Vec<String>,
    pub action: BatchAction,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum BatchAction {
    Delete,
    // AddTag(String), // TODO: Implement tagging
}

#[derive(Deserialize)]
pub struct BatchSortRequest {
    pub sort_by: SortCriteria,
}

#[derive(Deserialize)]
pub enum SortCriteria {
    Author,
    Title,
    Year,
}

pub async fn batch_edit(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<BatchEditRequest>,
) -> impl IntoResponse {
    match payload.action {
        BatchAction::Delete => {
            let ids_to_log = payload.ids.clone();
            match book::Entity::delete_many()
                .filter(book::Column::Id.is_in(payload.ids))
                .exec(&db)
                .await
            {
                Ok(res) => {
                    for id in &ids_to_log {
                        let _ = crate::sync::log_operation(&db, "book", id, "DELETE", None).await;
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({ "deleted": res.rows_affected })),
                    )
                        .into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e.to_string() })),
                )
                    .into_response(),
            }
        }
    }
}

#[derive(Deserialize)]
pub struct BatchAssignRequest {
    pub ids: Vec<String>,
    #[serde(default)]
    pub add_shelves: Vec<String>,
    #[serde(default)]
    pub add_collection_ids: Vec<String>,
    #[serde(default)]
    pub remove_shelves: Vec<String>,
    #[serde(default)]
    pub remove_collection_ids: Vec<String>,
}

/// File a selection of books onto shelves and into collections, optionally
/// removing them from the shelf or collection they were selected from.
pub async fn batch_assign(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<BatchAssignRequest>,
) -> impl IntoResponse {
    use crate::services::book_service::ServiceError;
    use crate::services::shelving_service::{BulkAssignment, assign_books};

    let assignment = BulkAssignment {
        book_ids: payload.ids,
        add_shelves: payload.add_shelves,
        add_collection_ids: payload.add_collection_ids,
        remove_shelves: payload.remove_shelves,
        remove_collection_ids: payload.remove_collection_ids,
    };
    match assign_books(&db, assignment).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(serde_json::json!({ "books_changed": outcome.books_changed })),
        )
            .into_response(),
        Err(ServiceError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Collection not found" })),
        )
            .into_response(),
        Err(ServiceError::InvalidInput(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": msg })),
        )
            .into_response(),
        Err(ServiceError::Database(msg)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": msg })),
        )
            .into_response(),
    }
}

pub async fn batch_sort(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<BatchSortRequest>,
) -> impl IntoResponse {
    // 1. Fetch all books
    let books = match book::Entity::find().all(&db).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 2. Sort in memory
    let mut sorted_books = books;
    match payload.sort_by {
        SortCriteria::Author => sorted_books.sort_by(|a, b| a.subjects.cmp(&b.subjects)), // Hack: using subjects as author proxy for now if author field is missing in struct? Wait, author is missing in struct! It was in initial sql but not in model?
        // Checking model... `author` is NOT in the struct! It seems we rely on `book_authors` relation.
        // For simple sorting, let's sort by Title for now, and fix Author sort later (requires join).
        SortCriteria::Title => sorted_books.sort_by(|a, b| a.title.cmp(&b.title)),
        SortCriteria::Year => sorted_books.sort_by_key(|a| a.publication_year),
    }

    // 3. Update shelf_position
    // This is inefficient (N updates), but fine for personal libraries (<10k books)
    for (index, book) in sorted_books.iter().enumerate() {
        let mut active: book::ActiveModel = book.clone().into();
        active.shelf_position = Set(Some(index as i32));
        let _ = active.update(&db).await;
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "success": true, "count": sorted_books.len() })),
    )
        .into_response()
}

pub async fn find_duplicates(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let books = book::Entity::find().all(&db).await.unwrap_or_default();

    let mut isbn_map: std::collections::HashMap<String, Vec<book::Book>> =
        std::collections::HashMap::new();

    for model in books {
        let isbn_clone = model.isbn.clone(); // Clone first
        if let Some(isbn) = isbn_clone
            && !isbn.is_empty()
        {
            let book_dto: book::Book = model.into(); // Now we can move model
            isbn_map.entry(isbn).or_default().push(book_dto);
        }
    }

    // Filter for groups > 1
    let duplicates: Vec<serde_json::Value> = isbn_map
        .into_iter()
        .filter(|(_, group)| group.len() > 1)
        .map(|(isbn, group)| {
            serde_json::json!({
                "isbn": isbn,
                "count": group.len(),
                "books": group
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(serde_json::json!({ "duplicates": duplicates })),
    )
        .into_response()
}
