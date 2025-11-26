use crate::models::tag::{self, Entity as Tag};
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
pub struct CreateTagRequest {
    name: String,
}

pub async fn list_tags(
    State(db): State<DatabaseConnection>,
) -> impl IntoResponse {
    let tags = Tag::find().all(&db).await.unwrap_or(vec![]);
    (StatusCode::OK, Json(tags)).into_response()
}

pub async fn create_tag(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<CreateTagRequest>,
) -> impl IntoResponse {
    let tag = tag::ActiveModel {
        name: Set(payload.name),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match tag.insert(&db).await {
        Ok(model) => (StatusCode::CREATED, Json(model)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub async fn get_tag(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let tag = Tag::find_by_id(id).one(&db).await.unwrap_or(None);
    match tag {
        Some(tag) => (StatusCode::OK, Json(tag)).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "Tag not found" }))).into_response(),
    }
}

pub async fn delete_tag(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let tag = Tag::find_by_id(id).one(&db).await.unwrap_or(None);
    match tag {
        Some(tag) => {
            let res = tag.delete(&db).await;
            match res {
                Ok(_) => (StatusCode::OK, Json(json!({ "message": "Tag deleted" }))).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
            }
        }
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "Tag not found" }))).into_response(),
    }
}
