use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, QueryOrder, Set};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::collection;
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
    let collections = collection::Entity::find()
        .order_by_desc(collection::Column::CreatedAt)
        .all(&db)
        .await;

    match collections {
        Ok(cols) => {
            let mut dtos = Vec::new();
            for col in cols {
                // Placeholder counts for now
                let total = 0;
                let owned = 0;

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

// TODO: Implement import logic
pub async fn import_collection(
    State(_db): State<DatabaseConnection>,
    // Multipart or JSON payload for file/content
) -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, "Not implemented yet").into_response()
}
