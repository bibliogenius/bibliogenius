use crate::auth::{create_jwt, hash_password, verify_password};
use crate::models::user::{self, Entity as User};
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
}

pub async fn login(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    tracing::info!("Login attempt for user: {}", payload.username);

    let user = match user::Entity::find()
        .filter(user::Column::Username.eq(&payload.username))
        .one(&db)
        .await
    {
        Ok(Some(u)) => u,
        _ => {
            tracing::warn!("User not found: {}", payload.username);
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid credentials" })),
            )
                .into_response();
        }
    };

    tracing::debug!("User found: {}", user.username);

    match verify_password(&payload.password, &user.password_hash) {
        Ok(true) => {
            tracing::info!("Password verified successfully for user: {}", user.username);
            let token = create_jwt(&user.username, &user.role).unwrap();
            (StatusCode::OK, Json(json!({ "token": token }))).into_response()
        }
        _ => {
            tracing::warn!("Password verification failed for user: {}", user.username);
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid credentials" })),
            )
                .into_response()
        }
    }
}

// Temporary helper to create admin user if not exists
#[derive(Deserialize)]
pub struct CreateUserRequest {
    username: String,
    password: String,
}

pub async fn create_admin(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<CreateUserRequest>,
) -> impl IntoResponse {
    let password_hash = hash_password(&payload.password).unwrap();

    let user = user::ActiveModel {
        username: Set(payload.username),
        password_hash: Set(password_hash),
        role: Set("admin".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match user.insert(&db).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(json!({ "message": "Admin created" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
