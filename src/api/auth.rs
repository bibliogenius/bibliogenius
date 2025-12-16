use crate::auth::{create_jwt, hash_password, verify_password};
use crate::models::user;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use totp_rs::{Algorithm, Secret, TOTP};

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    token: String,
}

// --- Login with MFA check ---
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
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid credentials" })),
            )
                .into_response();
        }
    };

    match verify_password(&payload.password, &user.password_hash) {
        Ok(true) => {
            // Check if user has MFA enabled
            if user.totp_secret.is_some() {
                return (
                    StatusCode::FORBIDDEN, // Or PRECONDITION_REQUIRED
                    Json(json!({ "error": "mfa_required" })),
                )
                    .into_response();
            }

            let token = create_jwt(&user.username, &user.role).unwrap();
            (StatusCode::OK, Json(json!({ "token": token }))).into_response()
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid credentials" })),
        )
            .into_response(),
    }
}

// --- MFA Login ---
#[derive(Deserialize)]
pub struct MfaLoginRequest {
    username: String,
    password: String,
    code: String,
}

pub async fn login_mfa(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<MfaLoginRequest>,
) -> impl IntoResponse {
    let user = match user::Entity::find()
        .filter(user::Column::Username.eq(&payload.username))
        .one(&db)
        .await
    {
        Ok(Some(u)) => u,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid credentials" })),
            )
                .into_response()
        }
    };

    if verify_password(&payload.password, &user.password_hash).unwrap_or(false) {
        if let Some(secret_str) = user.totp_secret {
            // Secret stored as encoded string (likely base32 if we use get_secret_base32)
            // But we store it as String. Let's assume we store the Base32 string.
            // Setup saves `secret` which is `totp.get_secret_base32()`.
            // So here we need to use Secret::Encoded.
            let secret = Secret::Encoded(secret_str);
            let totp = TOTP::new(
                Algorithm::SHA1,
                6,
                1,
                30,
                secret.to_bytes().unwrap(),
                Some(payload.username.clone()),
                "BiblioGenius".to_string(),
            )
            .unwrap();

            if totp.check_current(&payload.code).unwrap_or(false) {
                let token = create_jwt(&user.username, &user.role).unwrap();
                return (StatusCode::OK, Json(json!({ "token": token }))).into_response();
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "Invalid credentials or MFA code" })),
    )
        .into_response()
}

// --- MFA Setup ---

#[derive(Serialize)]
pub struct MfaSetupResponse {
    secret: String,
    qr: String,
}

pub async fn setup_2fa(
    State(_db): State<DatabaseConnection>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Extract user from token
    let token = headers
        .get("Authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .replace("Bearer ", "");
    let claims = crate::auth::decode_jwt(&token).unwrap();
    let username = claims.sub;

    let secret = Secret::generate_secret();
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret.to_bytes().unwrap(),
        Some(username),
        "BiblioGenius".to_string(),
    )
    .unwrap();

    let secret_str = totp.get_secret_base32();
    let qr = totp.get_qr_base64().unwrap();

    (
        StatusCode::OK,
        Json(MfaSetupResponse {
            secret: secret_str,
            qr,
        }),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct MfaVerifyRequest {
    secret: String,
    code: String,
}

pub async fn verify_2fa(
    State(db): State<DatabaseConnection>,
    headers: axum::http::HeaderMap,
    Json(payload): Json<MfaVerifyRequest>,
) -> impl IntoResponse {
    let token = headers
        .get("Authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .replace("Bearer ", "");
    let claims = crate::auth::decode_jwt(&token).unwrap();

    // Verify code against secret
    // Payload secret is the one sent from setup (Base32 string)
    let secret = Secret::Encoded(payload.secret.clone());
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret.to_bytes().unwrap(),
        Some(claims.sub.clone()),
        "BiblioGenius".to_string(),
    )
    .unwrap();

    if !totp.check_current(&payload.code).unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid code" })),
        )
            .into_response();
    }

    // Save secret to DB (as Base32 string)
    let user = user::Entity::find()
        .filter(user::Column::Username.eq(&claims.sub))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut user: user::ActiveModel = user.into();
    user.totp_secret = Set(Some(payload.secret));
    user.update(&db).await.unwrap();

    (StatusCode::OK, Json(json!({ "message": "MFA enabled" }))).into_response()
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

#[derive(Serialize)]
pub struct MeResponse {
    pub user_id: i32,
    pub username: String,
    pub library_id: i32,
    pub role: String,
    pub mfa_enabled: bool,
}

pub async fn get_me(
    State(db): State<DatabaseConnection>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // 1. Extract token
    let auth_header = headers.get("Authorization");
    if auth_header.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Missing token"})),
        )
            .into_response();
    }

    let token = auth_header
        .unwrap()
        .to_str()
        .unwrap()
        .replace("Bearer ", "");

    // 2. Decode token
    let claims = match crate::auth::decode_jwt(&token) {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid token"})),
            )
                .into_response()
        }
    };

    // 3. Fetch User
    let user = match user::Entity::find()
        .filter(user::Column::Username.eq(&claims.sub))
        .one(&db)
        .await
    {
        Ok(Some(u)) => u,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "User not found"})),
            )
                .into_response()
        }
    };

    // 4. Fetch Library (first library owned by user)
    use crate::models::library;

    let library = match library::Entity::find()
        .filter(library::Column::OwnerId.eq(user.id))
        .one(&db)
        .await
    {
        Ok(Some(l)) => l,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "User has no library"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    (
        StatusCode::OK,
        Json(MeResponse {
            user_id: user.id,
            username: user.username,
            library_id: library.id,
            role: user.role,
            mfa_enabled: user.totp_secret.is_some(),
        }),
    )
        .into_response()
}
