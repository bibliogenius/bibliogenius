//! SSRF Protection Tests: cover_proxy peer-registration check.
//!
//! validate_url() alone blocks loopback/link-local/multicast but allows
//! RFC1918 (needed for LAN peer discovery). Without a second check, a
//! caller could target router admin pages, NAS, printers, or any service
//! reachable on the local network. ensure_registered_peer() constrains
//! peer_url to URLs already vetted by the user via pairing.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rust_lib_app::api::peer::{cover_proxy, ensure_registered_peer};
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::peer;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tower::ServiceExt;

async fn setup_db_with_peer(stored_url: &str) -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    let now = chrono::Utc::now().to_rfc3339();
    let peer = peer::ActiveModel {
        name: Set("Test Peer".to_string()),
        url: Set(stored_url.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    peer.insert(&db).await.expect("insert peer");
    db
}

fn build_app(db: DatabaseConnection) -> axum::Router {
    axum::Router::new()
        .route("/peers/cover-proxy", axum::routing::get(cover_proxy))
        .with_state(AppState::new(db))
}

#[tokio::test]
async fn registered_peer_exact_match_accepted() {
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.100:8000").await;
    assert!(result.is_ok(), "exact URL match must be accepted");
}

#[tokio::test]
async fn registered_peer_db_no_slash_request_with_slash_accepted() {
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.100:8000/").await;
    assert!(
        result.is_ok(),
        "request with trailing slash must match DB entry without"
    );
}

#[tokio::test]
async fn registered_peer_db_with_slash_request_no_slash_accepted() {
    let db = setup_db_with_peer("http://192.168.1.100:8000/").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.100:8000").await;
    assert!(
        result.is_ok(),
        "request without trailing slash must match DB entry with"
    );
}

#[tokio::test]
async fn unregistered_peer_rejected() {
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.200:8000").await;
    assert_eq!(
        result.err(),
        Some(StatusCode::FORBIDDEN),
        "unknown URL must be rejected with 403"
    );
}

#[tokio::test]
async fn ssrf_rfc1918_target_not_registered_rejected() {
    // Classic SSRF scenario: attacker knows app runs on LAN and tries to
    // proxy through cover_proxy to probe the router admin page.
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.1").await;
    assert_eq!(
        result.err(),
        Some(StatusCode::FORBIDDEN),
        "router admin IP must be rejected even though RFC1918 is allowed by validate_url"
    );
}

#[tokio::test]
async fn handler_returns_403_when_peer_not_registered() {
    // End-to-end: the handler must short-circuit BEFORE any outbound HTTP call
    // when the peer is not registered, even if the URL would pass validate_url.
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let app = build_app(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/peers/cover-proxy?peer_url=http%3A%2F%2F192.168.1.1&book_id=42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn handler_returns_400_on_invalid_url() {
    // Regression: validate_url runs first and rejects loopback with 400 before
    // the DB lookup. The peer-registration check must not alter this behavior.
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let app = build_app(db);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/peers/cover-proxy?peer_url=http%3A%2F%2F127.0.0.1%3A8000&book_id=42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
