//! Integration tests for the `app_version` field on the hub profile register/update path.
//!
//! Verifies:
//!   - When `RegisterParams.app_version` is `Some`, the JSON body sent to
//!     `POST /api/directory/profile` includes `"app_version": "<v>"`.
//!   - When `None`, the field is fully absent from the body (never `null`),
//!     preserving backward-compat with the hub's `array_key_exists` handler.

use rust_lib_app::db;
use rust_lib_app::services::hub_directory_service::{HubDirectoryService, RegisterParams};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

fn base_params() -> RegisterParams {
    RegisterParams {
        node_id: "node-abc".to_string(),
        display_name: "Test Library".to_string(),
        book_count: 0,
        is_listed: false,
        requires_approval: true,
        accept_from: "everyone".to_string(),
        allow_borrowing: true,
        ..Default::default()
    }
}

fn profile_json(node_id: &str) -> serde_json::Value {
    serde_json::json!({
        "node_id": node_id,
        "display_name": "Test Library",
        "description": null,
        "book_count": 0,
        "location_country": null,
        "requires_approval": true,
        "allow_borrowing": true,
        "last_seen_at": null,
        "write_token": "tok-fresh-abc",
        "view_count": 0,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn register_includes_app_version_when_set() {
    let db = setup_test_db().await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_json("node-abc")))
        .expect(1)
        .mount(&hub)
        .await;

    let mut params = base_params();
    params.app_version = Some("0.9.0-alpha.1+422".to_string());

    let svc = HubDirectoryService::new();
    svc.register_or_update(&db, params)
        .await
        .expect("register_or_update succeeds");

    let received = hub.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("body is JSON");

    assert_eq!(
        body.get("app_version").and_then(|v| v.as_str()),
        Some("0.9.0-alpha.1+422"),
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn register_omits_app_version_when_none() {
    let db = setup_test_db().await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_json("node-abc")))
        .expect(1)
        .mount(&hub)
        .await;

    let params = base_params(); // app_version defaults to None

    let svc = HubDirectoryService::new();
    svc.register_or_update(&db, params)
        .await
        .expect("register_or_update succeeds");

    let received = hub.received_requests().await.expect("requests recorded");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("body is JSON");

    // Field must be fully absent, not present-with-null. The hub uses
    // `array_key_exists` to preserve prior values; a null would wipe them.
    assert!(
        body.get("app_version").is_none(),
        "expected app_version to be absent, got {:?}",
        body.get("app_version"),
    );
}
