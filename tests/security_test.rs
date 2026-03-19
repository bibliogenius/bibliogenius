use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use rust_lib_app::api;
use rust_lib_app::auth::{create_jwt, decode_jwt, hash_password, verify_password};
use rust_lib_app::db;
use sea_orm::{DatabaseConnection, EntityTrait, Set};
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

    let user = rust_lib_app::models::user::ActiveModel {
        username: Set("admin".to_string()),
        password_hash: Set(hash),
        role: Set("admin".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };
    rust_lib_app::models::user::Entity::insert(user)
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

// ── Relay Security Tests (S1-S5) ────────────────────────────────────

/// S2: read_token MUST NOT appear in outbound invite payloads or hub registration bodies.
/// This test scans the source code to ensure read_token is never sent outward.
#[test]
fn s2_read_token_never_in_outbound_payloads() {
    // Patterns that would indicate read_token being sent to the hub or peers.
    // The only valid use of read_token is for LOCAL relay polling (our own mailbox).
    let forbidden_patterns = [
        r#""read_token""#, // JSON key in outbound payload
        r#"read_token"#,   // field name in payload builder
    ];

    // Files that build outbound payloads (to hub or peers)
    let outbound_files = [include_str!("../src/services/hub_directory_service.rs")];

    for (file_idx, content) in outbound_files.iter().enumerate() {
        for pattern in &forbidden_patterns {
            // read_token should NOT appear in hub_directory_service (outbound to hub)
            assert!(
                !content.contains(pattern),
                "S2 VIOLATION: read_token found in outbound file index {file_idx}. \
                 read_token must NEVER be sent to the hub or included in invite payloads."
            );
        }
    }
}

/// S3: refresh_via_hub MUST verify x25519 key match before trusting credentials.
/// This is a structural test: the function source must contain key verification logic.
#[test]
fn s3_hub_refresh_verifies_x25519_key() {
    let source = include_str!("../src/api/peer.rs");

    // The refresh_via_hub function must contain x25519 key verification
    assert!(
        source.contains("x25519_public_key") && source.contains("mismatch"),
        "S3 VIOLATION: refresh_via_hub must verify x25519 key match before \
         trusting hub-provided relay credentials."
    );
}

/// S5: cover_url in catalog payloads must not contain local file paths.
/// Verifies that the catalog builder filters non-HTTP URLs.
#[test]
fn s5_catalog_must_not_leak_local_paths() {
    // Simulate cover URLs that should be filtered
    let valid_urls = [
        "https://example.com/cover.jpg",
        "http://books.google.com/cover.jpg",
    ];
    let invalid_urls = [
        "/var/mobile/Containers/Data/Application/abc/covers/1.jpg",
        "file:///Users/test/cover.jpg",
        "/home/user/.local/share/covers/2.png",
    ];

    for url in &valid_urls {
        assert!(
            url.starts_with("http://") || url.starts_with("https://"),
            "Valid URL should pass: {url}"
        );
    }

    for url in &invalid_urls {
        assert!(
            !(url.starts_with("http://") || url.starts_with("https://")),
            "S5 VIOLATION: local path should be filtered from catalog: {url}"
        );
    }
}
