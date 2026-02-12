//! JWT Authentication Edge Case Tests
//!
//! Covers: B9.1 JWT Token Validation, B9.2 Header Injection (TNR)
//! Extends the existing security_test.rs with adversarial scenarios.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use rust_lib_app::api;
use rust_lib_app::auth::{create_jwt, decode_jwt};
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use tower::util::ServiceExt;

async fn setup_test_state() -> AppState {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    AppState::new(db)
}

fn get_test_token() -> String {
    create_jwt("test_user", "admin").expect("Failed to create token")
}

// --- decode_jwt unit tests ---

#[test]
fn test_decode_malformed_token_rejected() {
    let result = decode_jwt("not.a.valid.jwt");
    assert!(result.is_err(), "Malformed token must be rejected");
}

#[test]
fn test_decode_empty_token_rejected() {
    let result = decode_jwt("");
    assert!(result.is_err(), "Empty token must be rejected");
}

#[test]
fn test_decode_random_string_rejected() {
    let result = decode_jwt("abc123xyz");
    assert!(result.is_err(), "Random string must be rejected");
}

#[test]
fn test_decode_token_with_wrong_signature() {
    // Create a valid-looking JWT but with garbage signature
    let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                 eyJzdWIiOiJ0ZXN0IiwiZXhwIjo5OTk5OTk5OTk5LCJyb2xlIjoiYWRtaW4ifQ.\
                 invalid_signature_here";
    let result = decode_jwt(token);
    assert!(
        result.is_err(),
        "Token with wrong signature must be rejected"
    );
}

#[test]
fn test_decode_expired_token_rejected() {
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    // Create a token that expired in the past
    let claims = json!({
        "sub": "test_user",
        "role": "admin",
        "exp": 1_000_000  // Year 1970 — long expired
    });

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"secret"), // Same secret as debug mode
    )
    .unwrap();

    let result = decode_jwt(&token);
    assert!(result.is_err(), "Expired token must be rejected");
}

#[test]
fn test_decode_token_signed_with_different_secret() {
    use jsonwebtoken::{EncodingKey, Header, encode};

    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        role: String,
        exp: usize,
    }

    let claims = Claims {
        sub: "hacker".to_string(),
        role: "admin".to_string(),
        exp: 9_999_999_999,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"wrong_secret_key"),
    )
    .unwrap();

    let result = decode_jwt(&token);
    assert!(
        result.is_err(),
        "Token signed with different secret must be rejected"
    );
}

// --- HTTP-level Claims extractor tests ---
//
// Use create_book (POST /books) which requires Claims extractor.
// Invalid auth → 401 (extractor rejects before handler runs).
// Valid auth → non-401 (auth passes; body parsing may fail, but that's OK).

fn build_protected_app(state: AppState) -> Router {
    Router::new()
        .route("/books", axum::routing::post(api::books::create_book))
        .with_state(state)
}

#[tokio::test]
async fn test_missing_authorization_header_returns_401() {
    let state = setup_test_state().await;
    let app = build_protected_app(state);

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_invalid_bearer_format_returns_401() {
    let state = setup_test_state().await;
    let app = build_protected_app(state);

    // "Token" instead of "Bearer"
    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, "Token abc123")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_expired_token_returns_401_via_http() {
    let state = setup_test_state().await;
    let app = build_protected_app(state);

    // Build an expired token
    use jsonwebtoken::{EncodingKey, Header, encode};
    let claims = serde_json::json!({
        "sub": "test_user",
        "role": "admin",
        "exp": 1_000_000
    });
    let expired_token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"secret"),
    )
    .unwrap();

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", expired_token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_valid_token_passes_auth_check() {
    let state = setup_test_state().await;
    let token = get_test_token();
    let app = build_protected_app(state);

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, format!("Bearer {}", token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    // Auth passes — response may be 400/422 (missing fields) but NOT 401
    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "Valid token must pass the Claims extractor"
    );
}

#[tokio::test]
async fn test_garbage_bearer_token_returns_401() {
    let state = setup_test_state().await;
    let app = build_protected_app(state);

    let req = Request::builder()
        .uri("/books")
        .method("POST")
        .header(header::AUTHORIZATION, "Bearer garbage_not_a_jwt")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
