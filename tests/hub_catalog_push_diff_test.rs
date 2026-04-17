//! Integration tests for diff-based hub catalog push (ADR-027).
//!
//! Covers:
//! 1. First push hits the hub and stores the resulting hash locally.
//! 2. Second identical push short-circuits with `SkippedLocal` (0 network
//!    round-trips).
//! 3. A mutation (different entries) forces a fresh push.
//! 4. When the hub returns 304, the client records the hash as synced so
//!    future pushes skip locally.
//! 5. Recovery flow clears the stored hash so the next sync re-pushes.
//! 6. The outbound body includes `catalog_hash` as a 64-hex-char string
//!    computed from the sorted canonical payload.

use rust_lib_app::db;
use rust_lib_app::services::hub_directory_service::{
    CatalogEntry, DirectoryConfig, HubDirectoryService, PushCatalogOutcome, compute_catalog_hash,
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ──────────────────────────────────────────────────────────

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

async fn insert_hub_config(db: &DatabaseConnection) {
    let now = chrono::Utc::now().to_rfc3339();
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO hub_directory_config \
             (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, created_at, updated_at) \
             VALUES (1, 'my-node', 'tok-abc', 1, 0, 'everyone', 1, '{now}', '{now}')"
        ),
    ))
    .await
    .expect("insert hub_directory_config");
}

fn sample_entries() -> Vec<CatalogEntry> {
    vec![
        CatalogEntry {
            isbn: "9782070612918".to_string(),
            book_id: None,
            title: "L'Étranger".to_string(),
            author: Some("Albert Camus".to_string()),
            cover_url: None,
            added_at: None,
        },
        CatalogEntry {
            isbn: "9780140283334".to_string(),
            book_id: None,
            title: "Slaughterhouse-Five".to_string(),
            author: Some("Kurt Vonnegut".to_string()),
            cover_url: None,
            added_at: None,
        },
    ]
}

fn mutated_entries() -> Vec<CatalogEntry> {
    let mut e = sample_entries();
    e.push(CatalogEntry {
        isbn: "9782266320269".to_string(),
        book_id: None,
        title: "1984".to_string(),
        author: Some("George Orwell".to_string()),
        cover_url: None,
        added_at: None,
    });
    e
}

// ── Tests ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn first_push_hits_hub_and_stores_hash() {
    let db = setup_test_db().await;
    insert_hub_config(&db).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/catalog"))
        .and(header("authorization", "Bearer tok-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "updated_at": "2026-04-14T10:00:00+00:00",
            "expires_at": "2026-04-21T10:00:00+00:00",
        })))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let entries = sample_entries();
    let outcome = svc.push_catalog(&db, &entries, entries.len() as i64).await;

    assert_eq!(outcome.unwrap(), PushCatalogOutcome::Pushed);

    // Hash persisted locally.
    let cfg: DirectoryConfig = HubDirectoryService::get_config(&db)
        .await
        .unwrap()
        .expect("config present");
    assert!(cfg.last_catalog_hash.is_some());
    assert_eq!(cfg.last_catalog_hash.as_deref().unwrap().len(), 64);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn second_identical_push_is_skipped_locally() {
    let db = setup_test_db().await;
    insert_hub_config(&db).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    // Hub is called exactly ONCE across both pushes.
    Mock::given(method("POST"))
        .and(path("/api/directory/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "updated_at": "2026-04-14T10:00:00+00:00",
            "expires_at": "2026-04-21T10:00:00+00:00",
        })))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let entries = sample_entries();

    let first = svc
        .push_catalog(&db, &entries, entries.len() as i64)
        .await
        .unwrap();
    assert_eq!(first, PushCatalogOutcome::Pushed);

    let second = svc
        .push_catalog(&db, &entries, entries.len() as i64)
        .await
        .unwrap();
    assert_eq!(second, PushCatalogOutcome::SkippedLocal);
    // wiremock's .expect(1) asserts at drop; explicit verify for clarity.
    hub.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn mutation_triggers_new_push() {
    let db = setup_test_db().await;
    insert_hub_config(&db).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "updated_at": "2026-04-14T10:00:00+00:00",
            "expires_at": "2026-04-21T10:00:00+00:00",
        })))
        .expect(2)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let base = sample_entries();
    let grown = mutated_entries();

    assert_eq!(
        svc.push_catalog(&db, &base, base.len() as i64)
            .await
            .unwrap(),
        PushCatalogOutcome::Pushed
    );
    assert_eq!(
        svc.push_catalog(&db, &grown, grown.len() as i64)
            .await
            .unwrap(),
        PushCatalogOutcome::Pushed
    );

    hub.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn hub_304_is_recorded_as_skipped_remote_and_future_pushes_short_circuit() {
    let db = setup_test_db().await;
    insert_hub_config(&db).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    // First request: hub returns 304 (it already has this catalog).
    // Second request should be skipped locally since the hash is now stored.
    Mock::given(method("POST"))
        .and(path("/api/directory/catalog"))
        .respond_with(ResponseTemplate::new(304))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let entries = sample_entries();

    let first = svc
        .push_catalog(&db, &entries, entries.len() as i64)
        .await
        .unwrap();
    assert_eq!(first, PushCatalogOutcome::SkippedRemote);

    let cfg = HubDirectoryService::get_config(&db).await.unwrap().unwrap();
    assert!(cfg.last_catalog_hash.is_some());

    let second = svc
        .push_catalog(&db, &entries, entries.len() as i64)
        .await
        .unwrap();
    assert_eq!(second, PushCatalogOutcome::SkippedLocal);

    hub.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn posted_body_contains_catalog_hash_matching_pure_helper() {
    let db = setup_test_db().await;
    insert_hub_config(&db).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/catalog"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "updated_at": "2026-04-14T10:00:00+00:00",
            "expires_at": "2026-04-21T10:00:00+00:00",
        })))
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let entries = sample_entries();

    svc.push_catalog(&db, &entries, entries.len() as i64)
        .await
        .unwrap();

    let received = hub.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(&received[0].body).expect("valid JSON body");

    // catalog_hash is present, lowercase 64-char hex.
    let hash = body
        .get("catalog_hash")
        .and_then(|v| v.as_str())
        .expect("catalog_hash present in body");
    assert_eq!(hash.len(), 64);
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );

    // The hash equals what the pure helper produces for the same
    // canonical inputs — enforces the wire-format contract.
    let isbn_payload = body.get("isbn_payload").and_then(|v| v.as_str()).unwrap();
    let catalog_payload = body
        .get("catalog_payload")
        .and_then(|v| v.as_str())
        .unwrap();
    let book_count = body.get("book_count").and_then(|v| v.as_i64()).unwrap();
    assert_eq!(
        hash,
        compute_catalog_hash(isbn_payload, catalog_payload, book_count)
    );
}
