use crate::models::user::Entity as User;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::*;
use serde_json::json;

pub async fn list_users(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let users = User::find().all(&db).await.unwrap_or(vec![]);
    (StatusCode::OK, Json(users)).into_response()
}

pub async fn get_user(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let user = User::find_by_id(id).one(&db).await.unwrap_or(None);
    match user {
        Some(user) => (StatusCode::OK, Json(user)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "User not found" })),
        )
            .into_response(),
    }
}

pub async fn delete_user(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let user = User::find_by_id(id).one(&db).await.unwrap_or(None);
    match user {
        Some(user) => {
            let res = user.delete(&db).await;
            match res {
                Ok(_) => {
                    (StatusCode::OK, Json(json!({ "message": "User deleted" }))).into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
                    .into_response(),
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "User not found" })),
        )
            .into_response(),
    }
}
