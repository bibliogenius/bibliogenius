//! MFA / TOTP Tests
//!
//! Covers: A1.15-A1.20, B10.1 TOTP Robustness (TNR)
//! Tests 2FA setup, code verification, and MFA login flow.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use rust_lib_app::api;
use rust_lib_app::auth::{create_jwt, hash_password};
use rust_lib_app::db;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use totp_rs::{Algorithm, Secret, TOTP};
use tower::util::ServiceExt;

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

async fn create_test_user(db: &DatabaseConnection, password: &str) {
    let hash = hash_password(password).unwrap();
    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("testuser".to_string()),
        password_hash: Set(hash),
        role: Set("admin".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    user.insert(db).await.expect("Failed to create user");
}

async fn create_user_with_totp(db: &DatabaseConnection, password: &str) -> String {
    let hash = hash_password(password).unwrap();

    // Generate a TOTP secret
    let secret = Secret::generate_secret();
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret.to_bytes().unwrap(),
        Some("testuser".to_string()),
        "BiblioGenius".to_string(),
    )
    .unwrap();
    let secret_b32 = totp.get_secret_base32();

    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("testuser".to_string()),
        password_hash: Set(hash),
        role: Set("admin".to_string()),
        totp_secret: Set(Some(secret_b32.clone())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    user.insert(db).await.expect("Failed to create user");
    secret_b32
}

fn build_auth_router(db: DatabaseConnection) -> Router {
    Router::new()
        .route("/auth/login", axum::routing::post(api::auth::login))
        .route("/auth/login-mfa", axum::routing::post(api::auth::login_mfa))
        .route("/auth/2fa/setup", axum::routing::post(api::auth::setup_2fa))
        .route(
            "/auth/2fa/verify",
            axum::routing::post(api::auth::verify_2fa),
        )
        .with_state(db)
}

async fn parse_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

// --- 2FA Setup ---

#[tokio::test]
async fn test_setup_2fa_returns_secret_and_qr() {
    let db = setup_test_db().await;
    create_test_user(&db, "password123").await;
    let token = create_jwt("testuser", "admin").unwrap();
    let app = build_auth_router(db);

    let req = Request::builder()
        .uri("/auth/2fa/setup")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = parse_json(response).await;
    assert!(
        json["secret"].as_str().is_some(),
        "Response must contain a secret"
    );
    assert!(
        !json["secret"].as_str().unwrap().is_empty(),
        "Secret must not be empty"
    );
    let qr = json["qr"].as_str().unwrap();
    assert!(!qr.is_empty(), "QR base64 string must not be empty");
}

#[tokio::test]
async fn test_setup_2fa_requires_auth() {
    let db = setup_test_db().await;
    let app = build_auth_router(db);

    let req = Request::builder()
        .uri("/auth/2fa/setup")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// --- 2FA Verify ---

#[tokio::test]
async fn test_verify_2fa_valid_code_accepted() {
    let db = setup_test_db().await;
    create_test_user(&db, "password123").await;
    let token = create_jwt("testuser", "admin").unwrap();
    let app = build_auth_router(db);

    // Generate a TOTP secret and compute current code
    let secret = Secret::generate_secret();
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret.to_bytes().unwrap(),
        Some("testuser".to_string()),
        "BiblioGenius".to_string(),
    )
    .unwrap();
    let secret_b32 = totp.get_secret_base32();
    let valid_code = totp.generate_current().unwrap();

    let payload = serde_json::json!({
        "secret": secret_b32,
        "code": valid_code
    });

    let req = Request::builder()
        .uri("/auth/2fa/verify")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = parse_json(response).await;
    assert_eq!(json["message"], "MFA enabled");
}

#[tokio::test]
async fn test_verify_2fa_invalid_code_rejected() {
    let db = setup_test_db().await;
    create_test_user(&db, "password123").await;
    let token = create_jwt("testuser", "admin").unwrap();
    let app = build_auth_router(db);

    let secret = Secret::generate_secret();
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret.to_bytes().unwrap(),
        Some("testuser".to_string()),
        "BiblioGenius".to_string(),
    )
    .unwrap();
    let secret_b32 = totp.get_secret_base32();

    let payload = serde_json::json!({
        "secret": secret_b32,
        "code": "000000"  // Almost certainly wrong
    });

    let req = Request::builder()
        .uri("/auth/2fa/verify")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = parse_json(response).await;
    assert_eq!(json["error"], "Invalid code");
}

// --- Login with MFA ---

#[tokio::test]
async fn test_login_returns_mfa_required_when_2fa_enabled() {
    let db = setup_test_db().await;
    create_user_with_totp(&db, "password123").await;
    let app = build_auth_router(db);

    let payload = serde_json::json!({
        "username": "testuser",
        "password": "password123"
    });

    let req = Request::builder()
        .uri("/auth/login")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "Login should return 403 when MFA is enabled"
    );

    let json = parse_json(response).await;
    assert_eq!(json["error"], "mfa_required");
}

#[tokio::test]
async fn test_login_mfa_with_valid_code_returns_token() {
    let db = setup_test_db().await;
    let secret_b32 = create_user_with_totp(&db, "password123").await;
    let app = build_auth_router(db);

    // Generate current TOTP code
    let secret = Secret::Encoded(secret_b32);
    let totp = TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        secret.to_bytes().unwrap(),
        Some("testuser".to_string()),
        "BiblioGenius".to_string(),
    )
    .unwrap();
    let valid_code = totp.generate_current().unwrap();

    let payload = serde_json::json!({
        "username": "testuser",
        "password": "password123",
        "code": valid_code
    });

    let req = Request::builder()
        .uri("/auth/login-mfa")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = parse_json(response).await;
    assert!(
        json["token"].as_str().is_some(),
        "MFA login must return a JWT token"
    );
}

#[tokio::test]
async fn test_login_mfa_with_invalid_code_rejected() {
    let db = setup_test_db().await;
    create_user_with_totp(&db, "password123").await;
    let app = build_auth_router(db);

    let payload = serde_json::json!({
        "username": "testuser",
        "password": "password123",
        "code": "999999"
    });

    let req = Request::builder()
        .uri("/auth/login-mfa")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_login_mfa_with_wrong_password_rejected() {
    let db = setup_test_db().await;
    create_user_with_totp(&db, "password123").await;
    let app = build_auth_router(db);

    let payload = serde_json::json!({
        "username": "testuser",
        "password": "wrong_password",
        "code": "123456"
    });

    let req = Request::builder()
        .uri("/auth/login-mfa")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
