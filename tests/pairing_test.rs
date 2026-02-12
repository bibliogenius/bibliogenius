//! Pairing Code Tests
//!
//! Covers: B13.1 Pairing Code (TNR)
//! Tests the device pairing flow: code generation, verification, and expiration.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use rust_lib_app::api;
use rust_lib_app::db;
use sea_orm::DatabaseConnection;
use tower::util::ServiceExt;

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

fn build_pairing_router(db: DatabaseConnection) -> Router {
    Router::new()
        .route(
            "/auth/pairing/code",
            axum::routing::post(api::auth::pairing_generate_code),
        )
        .route(
            "/auth/pairing/verify",
            axum::routing::post(api::auth::pairing_verify_code),
        )
        .with_state(db)
}

async fn parse_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn test_generate_code_returns_6_digits() {
    let db = setup_test_db().await;
    let app = build_pairing_router(db);

    let payload = serde_json::json!({
        "uuid": "test-library-uuid",
        "secret": "sync-secret-123",
        "ip": "192.168.1.100"
    });

    let req = Request::builder()
        .uri("/auth/pairing/code")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = parse_json(response).await;
    let code = json["code"].as_str().unwrap();
    assert_eq!(code.len(), 6, "Pairing code must be 6 digits");
    assert!(code.parse::<u32>().is_ok(), "Pairing code must be numeric");
    assert_eq!(json["expires_in"], 300, "Expiration must be 300 seconds");
}

#[tokio::test]
async fn test_generate_code_missing_uuid_returns_400() {
    let db = setup_test_db().await;
    let app = build_pairing_router(db);

    let payload = serde_json::json!({
        "secret": "sync-secret",
        "ip": "192.168.1.100"
    });

    let req = Request::builder()
        .uri("/auth/pairing/code")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = parse_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("uuid"));
}

#[tokio::test]
async fn test_verify_valid_code_returns_session_data() {
    let db = setup_test_db().await;
    let app = build_pairing_router(db);

    // Step 1: Generate code
    let gen_payload = serde_json::json!({
        "uuid": "my-library-uuid",
        "secret": "my-sync-secret",
        "ip": "192.168.1.50"
    });

    let req = Request::builder()
        .uri("/auth/pairing/code")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&gen_payload).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let gen_json = parse_json(response).await;
    let code = gen_json["code"].as_str().unwrap();

    // Step 2: Verify code
    let verify_payload = serde_json::json!({ "code": code });

    let req = Request::builder()
        .uri("/auth/pairing/verify")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&verify_payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = parse_json(response).await;
    assert_eq!(json["uuid"], "my-library-uuid");
    assert_eq!(json["secret"], "my-sync-secret");
    assert_eq!(json["ip"], "192.168.1.50");
}

#[tokio::test]
async fn test_verify_nonexistent_code_returns_404() {
    let db = setup_test_db().await;
    let app = build_pairing_router(db);

    let payload = serde_json::json!({ "code": "000000" });

    let req = Request::builder()
        .uri("/auth/pairing/verify")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let json = parse_json(response).await;
    assert_eq!(json["error"], "Invalid code");
}

#[tokio::test]
async fn test_verify_empty_code_returns_404() {
    let db = setup_test_db().await;
    let app = build_pairing_router(db);

    let payload = serde_json::json!({ "code": "" });

    let req = Request::builder()
        .uri("/auth/pairing/verify")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_multiple_codes_are_independent() {
    let db = setup_test_db().await;
    let app = build_pairing_router(db);

    // Generate two codes
    let payload_a = serde_json::json!({
        "uuid": "library-A",
        "secret": "secret-A",
        "ip": "10.0.0.1"
    });
    let payload_b = serde_json::json!({
        "uuid": "library-B",
        "secret": "secret-B",
        "ip": "10.0.0.2"
    });

    let req_a = Request::builder()
        .uri("/auth/pairing/code")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload_a).unwrap()))
        .unwrap();
    let resp_a = app.clone().oneshot(req_a).await.unwrap();
    let json_a = parse_json(resp_a).await;
    let code_a = json_a["code"].as_str().unwrap().to_string();

    let req_b = Request::builder()
        .uri("/auth/pairing/code")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&payload_b).unwrap()))
        .unwrap();
    let resp_b = app.clone().oneshot(req_b).await.unwrap();
    let json_b = parse_json(resp_b).await;
    let code_b = json_b["code"].as_str().unwrap().to_string();

    // Verify code A returns library-A data
    let verify_a = serde_json::json!({ "code": code_a });
    let req = Request::builder()
        .uri("/auth/pairing/verify")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&verify_a).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = parse_json(resp).await;
    assert_eq!(json["uuid"], "library-A");

    // Verify code B returns library-B data
    let verify_b = serde_json::json!({ "code": code_b });
    let req = Request::builder()
        .uri("/auth/pairing/verify")
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&verify_b).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let json = parse_json(resp).await;
    assert_eq!(json["uuid"], "library-B");
}
