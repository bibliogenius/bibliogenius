//! Integration tests for the 401 auto-recovery path in
//! `HubDirectoryService::register_or_update`.
//!
//! A profile upsert may hit 401 when the local `write_token` drifts away
//! from the hub (reinstall, old build that purged `hub_directory_config`
//! on a same-URL relay re-setup, etc.). If a `recovery_code` is stored
//! locally (migration 064+), the service exchanges it for a fresh
//! `write_token` via `/api/directory/recover` and retries the upsert once
//! so the user never lands in a 401 loop.

use rust_lib_app::db;
use rust_lib_app::services::hub_directory_service::{HubDirectoryService, RegisterParams};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Seed `hub_directory_config` as if the client had previously registered.
async fn seed_directory_config(
    db: &DatabaseConnection,
    node_id: &str,
    write_token: &str,
    recovery_code: Option<&str>,
) {
    let now = chrono::Utc::now().to_rfc3339();
    let rc_sql = match recovery_code {
        Some(rc) => format!("'{}'", rc.replace('\'', "''")),
        None => "NULL".to_string(),
    };
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO hub_directory_config
                 (id, node_id, write_token, is_listed, requires_approval,
                  accept_from, allow_borrowing, recovery_code,
                  created_at, updated_at)
             VALUES
                 (1, '{node_id}', '{write_token}', 0, 1,
                  'everyone', 1, {rc_sql},
                  '{now}', '{now}')"
        ),
    ))
    .await
    .expect("seed hub_directory_config");
}

async fn stored_write_token(db: &DatabaseConnection) -> Option<String> {
    db.query_one(Statement::from_string(
        db.get_database_backend(),
        "SELECT write_token FROM hub_directory_config WHERE id = 1".to_owned(),
    ))
    .await
    .unwrap()
    .and_then(|row| row.try_get::<String>("", "write_token").ok())
}

async fn stored_recovery_code(db: &DatabaseConnection) -> Option<String> {
    db.query_one(Statement::from_string(
        db.get_database_backend(),
        "SELECT recovery_code FROM hub_directory_config WHERE id = 1".to_owned(),
    ))
    .await
    .unwrap()
    .and_then(|row| row.try_get::<String>("", "recovery_code").ok())
}

fn base_params(node_id: &str) -> RegisterParams {
    RegisterParams {
        node_id: node_id.to_string(),
        display_name: "Eve's Library".to_string(),
        book_count: 70,
        is_listed: false,
        requires_approval: true,
        accept_from: "everyone".to_string(),
        allow_borrowing: true,
        ..Default::default()
    }
}

fn profile_json(node_id: &str, write_token: Option<&str>) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "node_id": node_id,
        "display_name": "Eve's Library",
        "description": null,
        "book_count": 70,
        "location_country": null,
        "requires_approval": true,
        "allow_borrowing": true,
        "last_seen_at": null,
        "view_count": 0,
    });
    if let Some(tok) = write_token {
        obj["write_token"] = serde_json::Value::String(tok.to_string());
    }
    obj
}

/// End-to-end self-heal: a stale write_token receives 401, the service
/// exchanges the stored recovery_code for a fresh write_token via
/// `/recover`, retries the upsert with the new Bearer, and persists the
/// new `recovery_code` returned by the hub.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn register_recovers_on_401_with_local_recovery_code() {
    let db = setup_test_db().await;
    let node_id = "26e4b4d9-acff-42cb-8b25-0bf32457a232";
    seed_directory_config(&db, node_id, "stale-token", Some("RC-OLD")).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    // 1. Initial upsert with the stale Bearer -> 401.
    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .and(header("authorization", "Bearer stale-token"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({ "error": "invalid token" })),
        )
        .expect(1)
        .mount(&hub)
        .await;

    // 2. /recover exchanges RC-OLD for a fresh token + fresh recovery code.
    Mock::given(method("POST"))
        .and(path("/api/directory/recover"))
        .and(body_partial_json(serde_json::json!({
            "node_id": node_id,
            "recovery_code": "RC-OLD",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "node_id": node_id,
            "display_name": "Eve's Library",
            "description": null,
            "book_count": 48,
            "location_country": null,
            "requires_approval": true,
            "allow_borrowing": true,
            "last_seen_at": null,
            "view_count": 0,
            "write_token": "fresh-token",
            "recovery_code": "RC-NEW",
        })))
        .expect(1)
        .mount(&hub)
        .await;

    // 3. Retry upsert with the fresh Bearer -> 200.
    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .and(header("authorization", "Bearer fresh-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_json(node_id, None)))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let result = svc
        .register_or_update(&db, base_params(node_id))
        .await
        .expect("register_or_update should self-heal via /recover");

    assert_eq!(result.write_token, "fresh-token");
    assert_eq!(result.recovery_code.as_deref(), Some("RC-NEW"));

    assert_eq!(
        stored_write_token(&db).await.as_deref(),
        Some("fresh-token"),
        "new write_token must be persisted locally",
    );
    assert_eq!(
        stored_recovery_code(&db).await.as_deref(),
        Some("RC-NEW"),
        "new recovery_code must replace the burned one",
    );
}

/// No recovery_code locally -> we can't self-heal; the 401 must surface
/// verbatim so an admin can intervene (delete hub profile, etc.). This is
/// the pre-migration-064 legacy case and must not regress.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn register_surfaces_401_when_no_recovery_code() {
    let db = setup_test_db().await;
    let node_id = "2d89e00b-dcd0-4485-82b1-5950888d6c9d";
    seed_directory_config(&db, node_id, "stale-token", None).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({ "error": "Valid write_token required" })),
        )
        .expect(1) // recover must NOT be attempted
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let err = svc
        .register_or_update(&db, base_params(node_id))
        .await
        .expect_err("register_or_update must not swallow the 401");

    let msg = format!("{err}");
    assert!(msg.contains("401"), "expected 401 to surface, got {msg}");
}

/// Recovery_code is invalid on the hub -> `/recover` 401s. We surface a
/// clear "auto-recovery failed" error so the caller can stop retrying and
/// surface the issue to the user (recovery code was rotated out-of-band,
/// or the profile was deleted server-side).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn register_reports_when_recovery_code_is_rejected() {
    let db = setup_test_db().await;
    let node_id = "7193342b-76a9-458d-82ee-e711bea11c7b";
    seed_directory_config(&db, node_id, "stale-token", Some("RC-BAD")).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&hub)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/directory/recover"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(serde_json::json!({ "error": "Invalid recovery code" })),
        )
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let err = svc
        .register_or_update(&db, base_params(node_id))
        .await
        .expect_err("recovery failure must surface");

    let msg = format!("{err}");
    assert!(msg.contains("401"), "expected 401 in error, got {msg}");
    assert!(
        msg.to_lowercase().contains("auto-recovery"),
        "expected auto-recovery mention, got {msg}"
    );

    // The stale creds must stay in place; /recover didn't rotate them.
    assert_eq!(
        stored_write_token(&db).await.as_deref(),
        Some("stale-token"),
    );
    assert_eq!(stored_recovery_code(&db).await.as_deref(), Some("RC-BAD"));
}
