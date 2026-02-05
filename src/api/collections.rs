//! Collection API handlers using repository pattern

use axum::{
    Json,
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, Set};
use serde::Deserialize;
use serde_json::json;

use crate::domain::{CreateCollectionInput, DomainError};
use crate::infrastructure::AppState;

/// List all collections with book counts
pub async fn list_collections(State(state): State<AppState>) -> impl IntoResponse {
    match state.collection_repo.find_all().await {
        Ok(collections) => (StatusCode::OK, Json(collections)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct CreateCollectionRequest {
    pub name: String,
    pub description: Option<String>,
    pub source: Option<String>,
}

/// Create a new collection
pub async fn create_collection(
    State(state): State<AppState>,
    Json(payload): Json<CreateCollectionRequest>,
) -> impl IntoResponse {
    let input = CreateCollectionInput {
        name: payload.name,
        description: payload.description,
        source: payload.source,
    };

    match state.collection_repo.create(input).await {
        Ok(collection) => (StatusCode::CREATED, Json(collection)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Get a single collection by ID
pub async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.collection_repo.find_by_id(&id).await {
        Ok(Some(collection)) => (StatusCode::OK, Json(collection)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Collection not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Delete a collection by ID
pub async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.collection_repo.delete(&id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(DomainError::NotFound) => {
            // Idempotent delete
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Get all books in a collection
pub async fn get_collection_books(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.collection_repo.get_books(&id).await {
        Ok(books) => (StatusCode::OK, Json(books)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Add a book to a collection
pub async fn add_book_to_collection(
    State(state): State<AppState>,
    Path((collection_id, book_id)): Path<(String, i32)>,
) -> impl IntoResponse {
    match state
        .collection_repo
        .add_book(&collection_id, book_id)
        .await
    {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Remove a book from a collection
pub async fn remove_book_from_collection(
    State(state): State<AppState>,
    Path((collection_id, book_id)): Path<(String, i32)>,
) -> impl IntoResponse {
    match state
        .collection_repo
        .remove_book(&collection_id, book_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// Get all collections a book belongs to
pub async fn get_book_collections(
    State(state): State<AppState>,
    Path(book_id): Path<i32>,
) -> impl IntoResponse {
    match state.collection_repo.get_book_collections(book_id).await {
        Ok(collections) => (StatusCode::OK, Json(collections)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateBookCollectionsRequest {
    pub collection_ids: Vec<String>,
}

/// Update which collections a book belongs to
pub async fn update_book_collections(
    State(state): State<AppState>,
    Path(book_id): Path<i32>,
    Json(payload): Json<UpdateBookCollectionsRequest>,
) -> impl IntoResponse {
    match state
        .collection_repo
        .update_book_collections(book_id, payload.collection_ids)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct ImportQuery {
    pub owned: Option<bool>,
}

/// Import books from file into a collection
/// Note: This handler uses direct DB access for complex book/copy creation logic
pub async fn import_collection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ImportQuery>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    use crate::import;
    use crate::models::{book, copy};

    let db = state.db();
    let import_as_owned = query.owned.unwrap_or(false);

    // Verify collection exists
    if state
        .collection_repo
        .find_by_id(&id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Collection not found"})),
        )
            .into_response();
    }

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        if field.name() == Some("file") {
            let data = field.bytes().await.unwrap_or_default();
            match import::parse_import_file(&data) {
                Ok(books) => {
                    let mut count = 0;
                    let mut errors = Vec::new();
                    for req in books {
                        let now = chrono::Utc::now();
                        // 1. Create Book
                        let new_book = book::ActiveModel {
                            title: Set(req.title.clone()),
                            isbn: Set(req.isbn),
                            summary: Set(None),
                            publisher: Set(req.publisher),
                            publication_year: Set(req.publication_year),
                            created_at: Set(now.to_rfc3339()),
                            updated_at: Set(now.to_rfc3339()),
                            owned: Set(import_as_owned),
                            ..Default::default()
                        };
                        match new_book.insert(db).await {
                            Ok(created_book) => {
                                // 2. Link to Collection via repository
                                if let Err(e) =
                                    state.collection_repo.add_book(&id, created_book.id).await
                                {
                                    errors.push(format!("Failed to link {}: {}", req.title, e));
                                    continue;
                                }
                                count += 1;

                                // 3. Create Copy if owned
                                if import_as_owned {
                                    let copy_model = copy::ActiveModel {
                                        book_id: Set(created_book.id),
                                        library_id: Set(1), // Default library ID
                                        status: Set("available".to_string()),
                                        is_temporary: Set(false),
                                        created_at: Set(now.to_rfc3339()),
                                        updated_at: Set(now.to_rfc3339()),
                                        ..Default::default()
                                    };
                                    let _ = copy_model.insert(db).await;
                                }
                            }
                            Err(e) => errors.push(format!("{}: {}", req.title, e)),
                        }
                    }
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "imported": count,
                            "errors": if errors.is_empty() { None } else { Some(errors) }
                        })),
                    )
                        .into_response();
                }
                Err(e) => {
                    return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
                }
            }
        }
    }
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "No file uploaded"})),
    )
        .into_response()
}
