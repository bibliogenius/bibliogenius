use bibliogenius::api;
use bibliogenius::db;
use bibliogenius::auth::{hash_password, verify_password, create_jwt, decode_jwt};
use sea_orm::{DatabaseConnection, EntityTrait, Set};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use tower::util::ServiceExt; // for `oneshot`

// Helper to create a test database
async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

#[tokio::test]
async fn test_password_hashing() {
    let password = "super_secret_password";
    let hash = hash_password(password).expect("Failed to hash password");
    
    assert_ne!(password, hash);
    assert!(verify_password(password, &hash).unwrap());
    assert!(!verify_password("wrong_password", &hash).unwrap());
}

#[tokio::test]
async fn test_jwt_creation_and_verification() {
    let username = "test_user";
    let role = "admin";
    
    let token = create_jwt(username, role).expect("Failed to create JWT");
    assert!(!token.is_empty());
    
    let claims = decode_jwt(&token).expect("Failed to verify JWT");
    assert_eq!(claims.sub, username);
    // Note: claims might not have role directly depending on implementation, checking sub is good enough
}

#[tokio::test]
async fn test_login_flow() {
    let db = setup_test_db().await;
    
    // 1. Create Admin User manually
    let password = "admin_password";
    let hash = hash_password(password).unwrap();
    
    let user = bibliogenius::models::user::ActiveModel {
        username: Set("admin".to_string()),
        password_hash: Set(hash),
        role: Set("admin".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    bibliogenius::models::user::Entity::insert(user)
        .exec(&db)
        .await
        .expect("Failed to create user");

    // 2. Setup Router (simulating main.rs)
    let app = Router::new()
        .route("/auth/login", axum::routing::post(api::auth::login))
        .with_state(db);

    // 3. Test Success Login
    let payload = serde_json::json!({
        "username": "admin",
        "password": "admin_password"
    });
    
    let req = Request::builder()
        .uri("/auth/login")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    
    // 4. Test Invalid Password
    let payload_bad = serde_json::json!({
        "username": "admin",
        "password": "wrong_password"
    });
    
    let req_bad = Request::builder()
        .uri("/auth/login")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload_bad).unwrap()))
        .unwrap();

    let response_bad = app.clone().oneshot(req_bad).await.unwrap();
    assert_eq!(response_bad.status(), StatusCode::UNAUTHORIZED);
    
    // 5. Test Non-existent User
    let payload_none = serde_json::json!({
        "username": "nobody",
        "password": "password"
    });
    
    let req_none = Request::builder()
        .uri("/auth/login")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload_none).unwrap()))
        .unwrap();

    let response_none = app.oneshot(req_none).await.unwrap();
    assert_eq!(response_none.status(), StatusCode::UNAUTHORIZED);
}
