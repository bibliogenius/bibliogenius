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
    claims: crate::auth::Claims,
) -> impl IntoResponse {
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
    claims: crate::auth::Claims,
    Json(payload): Json<MfaVerifyRequest>,
) -> impl IntoResponse {
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
    claims: crate::auth::Claims,
) -> impl IntoResponse {
    // 1. Fetch User
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
// --- Pairing / Device Linking ---

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;

// In-memory store for active pairing codes: code -> (uuid, secret, ip, created_at)
// Code: 6 digits (e.g. "123456")
struct PairingSession {
    uuid: String,
    secret: String, // Pairing/Sync secret
    ip: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

static PAIRING_CODES: Lazy<Mutex<HashMap<String, PairingSession>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Serialize)]
pub struct PairingCodeResponse {
    code: String,
    expires_in: u64,
}

/// Start Manual Pairing: Generate 6-digit code
pub async fn pairing_generate_code(
    State(_db): State<DatabaseConnection>,
    // TODO: Verify admin/owner permission via claims?
    // claims: crate::auth::Claims,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use rand::Rng;

    // Payload should contain current library_uuid and some secret (or we generate one)
    // Actually, "pairing_secret" is what we want to share.
    // Ideally user provides the UUID they want to share.
    let uuid = payload
        .get("uuid")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let secret = payload
        .get("secret")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // IP might be needed for direct connection
    let ip = payload
        .get("ip")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if uuid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Missing uuid"})),
        )
            .into_response();
    }

    // Generate 6-digit code
    let mut rng = rand::thread_rng();
    let code: u32 = rng.gen_range(100_000..999_999);
    let code_str = code.to_string();

    {
        let mut store = PAIRING_CODES.lock().unwrap();
        // Cleanup old codes (older than 5 min)
        let now = chrono::Utc::now();
        store.retain(|_, v| (now - v.created_at).num_minutes() < 5);

        store.insert(
            code_str.clone(),
            PairingSession {
                uuid,
                secret,
                ip,
                created_at: now,
            },
        );
    }

    (
        StatusCode::OK,
        Json(PairingCodeResponse {
            code: code_str,
            expires_in: 300, // 5 minutes
        }),
    )
        .into_response()
}

/// Verify Pairing Code (Target calls Source via HTTP)
/// Input: { "code": "123456" }
/// Output: { "uuid": "...", "secret": "..." }
pub async fn pairing_verify_code(
    State(_db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let code = payload
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let store = PAIRING_CODES.lock().unwrap();
    if let Some(session) = store.get(code) {
        // Verify expiration
        if (chrono::Utc::now() - session.created_at).num_minutes() >= 5 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Code expired"})),
            )
                .into_response();
        }

        return (
            StatusCode::OK,
            Json(json!({
                "uuid": session.uuid,
                "secret": session.secret,
                "ip": session.ip
            })),
        )
            .into_response();
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "Invalid code"})),
    )
        .into_response()
}
