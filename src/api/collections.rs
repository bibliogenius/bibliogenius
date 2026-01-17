use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, QueryOrder, RelationTrait, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{import, models::collection};
// use crate::models::book; // For syncing status

#[derive(Serialize)]
pub struct CollectionDto {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    // Calculated fields
    pub total_books: i64,
    pub owned_books: i64,
}

#[derive(Deserialize)]
pub struct CreateCollectionRequest {
    pub name: String,
    pub description: Option<String>,
    pub source: Option<String>,
}

pub async fn list_collections(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::collection_book;
    use sea_orm::{ColumnTrait, PaginatorTrait, QueryFilter};

    let collections = collection::Entity::find()
        .order_by_desc(collection::Column::CreatedAt)
        .all(&db)
        .await;

    match collections {
        Ok(cols) => {
            let mut dtos = Vec::new();
            for col in cols {
                // Count total books in collection
                let total = collection_book::Entity::find()
                    .filter(collection_book::Column::CollectionId.eq(&col.id))
                    .count(&db)
                    .await
                    .unwrap_or(0) as i64;

                // For owned books, we would need a join with books table.
                // Keeping it proportional to total for now if not easily available without join,
                // or just set to 0. Let's aim for total first as that's what shows on the card.
                let owned = total;

                dtos.push(CollectionDto {
                    id: col.id,
                    name: col.name,
                    description: col.description,
                    source: col.source,
                    created_at: col.created_at,
                    updated_at: col.updated_at,
                    total_books: total,
                    owned_books: owned,
                });
            }
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn create_collection(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<CreateCollectionRequest>,
) -> impl IntoResponse {
    let new_collection = collection::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        name: Set(payload.name),
        description: Set(payload.description),
        source: Set(payload.source.unwrap_or_else(|| "manual".to_string())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    match new_collection.insert(&db).await {
        Ok(col) => (StatusCode::CREATED, Json(col)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn get_collection(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let collection = collection::Entity::find_by_id(id).one(&db).await;

    match collection {
        Ok(Some(col)) => (StatusCode::OK, Json(col)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "Collection not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn delete_collection(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let res = collection::Entity::delete_by_id(id).exec(&db).await;

    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Serialize)]
pub struct CollectionBookDto {
    pub book_id: i32,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub added_at: String,
    pub is_owned: bool, // Derived from comparing with owned books
    pub digital_formats: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct ImportQuery {
    pub owned: Option<bool>,
}

pub async fn import_collection(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ImportQuery>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    use crate::models::{book, collection_book, copy};
    use sea_orm::Set;

    let import_as_owned = query.owned.unwrap_or(false);

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
                        match new_book.insert(&db).await {
                            Ok(created_book) => {
                                // 2. Link to Collection
                                let link = collection_book::ActiveModel {
                                    collection_id: Set(id.clone()),
                                    book_id: Set(created_book.id),
                                    added_at: Set(now.to_rfc3339()),
                                };
                                match link.insert(&db).await {
                                    Ok(_) => {
                                        count += 1;
                                        // 3. Create Copy if owned
                                        if import_as_owned {
                                            let copy = copy::ActiveModel {
                                                book_id: Set(created_book.id),
                                                library_id: Set(1), // Default library ID
                                                status: Set("available".to_string()),
                                                is_temporary: Set(false),
                                                created_at: Set(now.to_rfc3339()),
                                                updated_at: Set(now.to_rfc3339()),
                                                ..Default::default()
                                            };
                                            let _ = copy.insert(&db).await;
                                        }
                                    }
                                    Err(e) => {
                                        errors.push(format!("Failed to link {}: {}", req.title, e))
                                    }
                                }
                            }
                            Err(e) => errors.push(format!("{}: {}", req.title, e)),
                        }
                    }
                    return (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "imported": count,
                            "errors": if errors.is_empty() { None } else { Some(errors) }
                        })),
                    )
                        .into_response();
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": e })),
                    )
                        .into_response();
                }
            }
        }
    }
    (StatusCode::BAD_REQUEST, "No file uploaded").into_response()
}

pub async fn get_collection_books(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // We need to join collection_books with books to get details.
    // For now, let's assume valid collection.

    // This is a manual join because SeaORM relations are complex to setup quickly for JSON output
    // But since we defined models, we can try using find_with_related if possible,
    // or just raw SQL for speed/simplicity in this initial phase.
    // Given the hybrid architecture, let's stick to using the entities we defined.

    use crate::models::{book, collection_book};
    use sea_orm::{ColumnTrait, QueryFilter};

    // 1. Get all collection_book entries
    let collection_books = collection_book::Entity::find()
        .filter(collection_book::Column::CollectionId.eq(id.clone()))
        .all(&db)
        .await;

    match collection_books {
        Ok(c_books) => {
            let mut dtos = Vec::new();
            for cb in c_books {
                // 2. Fetch book details for each (N+1 query for now, optimization later)
                if let Ok(Some(b)) = book::Entity::find_by_id(cb.book_id).one(&db).await {
                    dtos.push(CollectionBookDto {
                        book_id: b.id,
                        title: b.title,
                        // TODO: Join with authors table to get actual author name
                        author: None,
                        cover_url: b.cover_url,
                        added_at: cb.added_at,
                        is_owned: b.owned,
                        digital_formats: b
                            .digital_formats
                            .and_then(|s| serde_json::from_str(&s).ok()),
                    });
                }
            }
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn remove_book_from_collection(
    State(db): State<DatabaseConnection>,
    Path((collection_id, book_id)): Path<(String, i32)>,
) -> impl IntoResponse {
    use crate::models::collection_book;
    use sea_orm::ColumnTrait;
    use sea_orm::QueryFilter;

    // Composite key deletion
    let del_result = collection_book::Entity::delete_many()
        .filter(collection_book::Column::CollectionId.eq(collection_id))
        .filter(collection_book::Column::BookId.eq(book_id))
        .exec(&db)
        .await;

    match del_result {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Add a book to a collection
pub async fn add_book_to_collection(
    State(db): State<DatabaseConnection>,
    Path((collection_id, book_id)): Path<(String, i32)>,
) -> impl IntoResponse {
    use crate::models::collection_book;
    use sea_orm::ColumnTrait;
    use sea_orm::QueryFilter;

    // Check if already exists
    let existing = collection_book::Entity::find()
        .filter(collection_book::Column::CollectionId.eq(&collection_id))
        .filter(collection_book::Column::BookId.eq(book_id))
        .one(&db)
        .await;

    if let Ok(Some(_)) = existing {
        return StatusCode::OK.into_response(); // Already exists
    }

    // Create new entry
    let new_entry = collection_book::ActiveModel {
        collection_id: Set(collection_id),
        book_id: Set(book_id),
        added_at: Set(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()),
    };

    match new_entry.insert(&db).await {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn get_book_collections(
    State(db): State<DatabaseConnection>,
    Path(book_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::{collection, collection_book};
    use sea_orm::{ColumnTrait, QueryFilter, QuerySelect};

    let result = collection::Entity::find()
        .join(
            sea_orm::JoinType::InnerJoin,
            collection_book::Relation::Collection.def().rev(),
        )
        .filter(collection_book::Column::BookId.eq(book_id))
        .all(&db)
        .await;

    match result {
        Ok(cols) => {
            let mut dtos = Vec::new();
            for col in cols {
                dtos.push(CollectionDto {
                    id: col.id,
                    name: col.name,
                    description: col.description,
                    source: col.source,
                    created_at: col.created_at,
                    updated_at: col.updated_at,
                    total_books: 0, // Not needed for this view
                    owned_books: 0,
                });
            }
            (StatusCode::OK, Json(dtos)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UpdateBookCollectionsRequest {
    pub collection_ids: Vec<String>,
}

pub async fn update_book_collections(
    State(db): State<DatabaseConnection>,
    Path(book_id): Path<i32>,
    Json(payload): Json<UpdateBookCollectionsRequest>,
) -> impl IntoResponse {
    use crate::models::collection_book;
    use sea_orm::{ColumnTrait, QueryFilter};

    // 1. Remove existing associations
    let _ = collection_book::Entity::delete_many()
        .filter(collection_book::Column::BookId.eq(book_id))
        .exec(&db)
        .await;

    // 2. Add new associations
    for col_id in payload.collection_ids {
        let new_entry = collection_book::ActiveModel {
            collection_id: Set(col_id),
            book_id: Set(book_id),
            added_at: Set(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()),
        };
        let _ = new_entry.insert(&db).await;
    }

    StatusCode::OK.into_response()
}
