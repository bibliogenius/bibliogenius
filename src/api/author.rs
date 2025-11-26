use crate::models::author::{self, Entity as Author};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sea_orm::*;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct CreateAuthorRequest {
    name: String,
}

pub async fn list_authors(
    State(db): State<DatabaseConnection>,
) -> impl IntoResponse {
    let authors = Author::find().all(&db).await.unwrap_or(vec![]);
    (StatusCode::OK, Json(authors)).into_response()
}

pub async fn create_author(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<CreateAuthorRequest>,
) -> impl IntoResponse {
    let author = author::ActiveModel {
        name: Set(payload.name),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match author.insert(&db).await {
        Ok(model) => (StatusCode::CREATED, Json(model)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn get_author(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let author = Author::find_by_id(id).one(&db).await.unwrap_or(None);
    match author {
        Some(author) => (StatusCode::OK, Json(author)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "Author not found" }))).into_response(),
    }
}

pub async fn delete_author(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let author = Author::find_by_id(id).one(&db).await.unwrap_or(None);
    match author {
        Some(author) => {
            let res = author.delete(&db).await;
            match res {
                Ok(_) => (StatusCode::OK, Json(json!({ "message": "Author deleted" }))).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
            }
        }
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "Author not found" }))).into_response(),
    }
}
