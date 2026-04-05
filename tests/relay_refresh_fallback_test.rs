//! Tests for relay credential refresh LAN -> Hub fallback.
//!
//! Covers:
//! 1. LAN unreachable + hub returns valid creds (same x25519) -> fallback works
//! 2. LAN unreachable + hub x25519 mismatch -> fallback rejected
//! 3. No library_uuid + LAN unreachable -> no fallback attempted
//! 4. LAN reachable -> hub not called

use rust_lib_app::db;
use rust_lib_app::models::peer;
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ──────────────────────────────────────────────────────────

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Insert hub_directory_config so refresh_via_hub can authenticate.
async fn insert_hub_config(db: &DatabaseConnection, write_token: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO hub_directory_config (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, created_at, updated_at) \
             VALUES (1, 'my-node-id', '{write_token}', 1, 0, 'everyone', 1, '{now}', '{now}')"
        ),
    ))
    .await
    .expect("insert hub_directory_config");
}

/// Create a peer::Model via DB insert and return it.
async fn insert_peer(
    db: &DatabaseConnection,
    name: &str,
    url: &str,
    library_uuid: Option<&str>,
    x25519_key: Option<&str>,
) -> peer::Model {
    let now = chrono::Utc::now().to_rfc3339();
    let model = peer::ActiveModel {
        name: Set(name.to_string()),
        url: Set(url.to_string()),
        library_uuid: Set(library_uuid.map(|s| s.to_string())),
        x25519_public_key: Set(x25519_key.map(|s| s.to_string())),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = peer::Entity::insert(model)
        .exec(db)
        .await
        .expect("insert peer");
    peer::Entity::find_by_id(res.last_insert_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

/// Build a HubProfile JSON response with relay credentials.
fn hub_profile_json(
    node_id: &str,
    x25519_key: &str,
    relay_url: &str,
    mailbox_id: &str,
    write_token: &str,
) -> serde_json::Value {
    serde_json::json!({
        "node_id": node_id,
        "display_name": "Test Peer",
        "book_count": 10,
        "requires_approval": false,
        "x25519_public_key": x25519_key,
        "relay_url": relay_url,
        "relay_mailbox_id": mailbox_id,
        "relay_write_token": write_token,
    })
}

// ── Tests ────────────────────────────────────────────────────────────

/// Test 1: LAN unreachable, hub returns valid creds with matching x25519
/// -> fallback works, credentials updated in DB.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn fallback_lan_fail_hub_ok() {
    let db = setup_test_db().await;

    // Start mock hub server
    let hub = MockServer::start().await;
    let peer_node_id = "peer-node-123";
    let x25519_key = "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    let new_relay_url = "https://hub.example.com";
    let new_mailbox = "a134ff9e-new-mailbox";
    let new_write_token = "new-write-tok";

    Mock::given(method("GET"))
        .and(path(format!("/api/directory/{peer_node_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(hub_profile_json(
            peer_node_id,
            x25519_key,
            new_relay_url,
            new_mailbox,
            new_write_token,
        )))
        .expect(1)
        .mount(&hub)
        .await;

    // Set HUB_URL to mock server
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    // Insert hub config for auth
    insert_hub_config(&db, "our-write-token").await;

    // Create peer with LAN URL (pointing to nothing -> LAN refresh will fail)
    let peer_model = insert_peer(
        &db,
        "iPhone Test",
        "http://192.168.1.99:8000", // unreachable
        Some(peer_node_id),
        Some(x25519_key),
    )
    .await;

    let result = rust_lib_app::api::peer::refresh_peer_relay_credentials(&db, &peer_model).await;

    assert!(result.is_some(), "Fallback to hub should succeed");
    let (r_url, r_mailbox, r_token) = result.unwrap();
    assert_eq!(r_url, new_relay_url);
    assert_eq!(r_mailbox, new_mailbox);
    assert_eq!(r_token, new_write_token);

    // Verify DB was updated
    let updated = peer::Entity::find_by_id(peer_model.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.relay_url.as_deref(), Some(new_relay_url));
    assert_eq!(updated.mailbox_id.as_deref(), Some(new_mailbox));
    assert_eq!(updated.relay_write_token.as_deref(), Some(new_write_token));
}

/// Test 2: LAN unreachable, hub returns creds with MISMATCHED x25519
/// -> fallback rejected (returns None).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn fallback_lan_fail_hub_x25519_mismatch() {
    let db = setup_test_db().await;

    let hub = MockServer::start().await;
    let peer_node_id = "peer-node-456";
    let local_x25519 = "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    let hub_x25519 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    Mock::given(method("GET"))
        .and(path(format!("/api/directory/{peer_node_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(hub_profile_json(
            peer_node_id,
            hub_x25519, // different from local
            "https://hub.example.com",
            "attacker-mailbox",
            "attacker-token",
        )))
        .expect(1)
        .mount(&hub)
        .await;

    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;

    let peer_model = insert_peer(
        &db,
        "Attacker Peer",
        "http://192.168.1.99:8000",
        Some(peer_node_id),
        Some(local_x25519),
    )
    .await;

    let result = rust_lib_app::api::peer::refresh_peer_relay_credentials(&db, &peer_model).await;

    assert!(
        result.is_none(),
        "Fallback must be rejected when x25519 keys mismatch"
    );

    // DB must NOT be updated with attacker credentials
    let unchanged = peer::Entity::find_by_id(peer_model.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(unchanged.relay_url.is_none());
    assert!(unchanged.mailbox_id.is_none());
}

/// Test 3: Peer has no library_uuid + LAN unreachable
/// -> no hub fallback attempted (returns None).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn no_fallback_without_library_uuid() {
    let db = setup_test_db().await;

    // No mock server needed -- hub should never be called
    let hub = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0) // MUST NOT be called
        .mount(&hub)
        .await;

    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;

    let peer_model = insert_peer(
        &db,
        "LAN-only Peer",
        "http://192.168.1.99:8000",
        None, // no library_uuid
        None,
    )
    .await;

    let result = rust_lib_app::api::peer::refresh_peer_relay_credentials(&db, &peer_model).await;

    assert!(
        result.is_none(),
        "Should return None without attempting hub fallback"
    );
}

/// Test 4: LAN reachable -> hub must NOT be called (no unnecessary request).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn lan_reachable_no_hub_fallback() {
    let db = setup_test_db().await;

    // LAN mock server returns valid config
    let lan = MockServer::start().await;
    let relay_url = "https://hub.example.com";
    let mailbox_id = "lan-mailbox-uuid";
    let write_token = "lan-write-tok";

    Mock::given(method("GET"))
        .and(path("/api/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "library_name": "iPhone Library",
            "library_description": null,
            "profile_type": "personal",
            "enabled_modules": [],
            "theme": "default",
            "share_location": false,
            "show_borrowed_books": false,
            "relay_url": relay_url,
            "mailbox_id": mailbox_id,
            "relay_write_token": write_token,
        })))
        .expect(1)
        .mount(&lan)
        .await;

    // Hub mock -- must NOT receive any requests
    let hub = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&hub)
        .await;

    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;

    let peer_model = insert_peer(
        &db,
        "LAN Peer",
        &lan.uri(), // reachable LAN URL
        Some("peer-node-789"),
        Some("aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344"),
    )
    .await;

    let result = rust_lib_app::api::peer::refresh_peer_relay_credentials(&db, &peer_model).await;

    assert!(result.is_some(), "LAN refresh should succeed");
    let (r_url, r_mailbox, r_token) = result.unwrap();
    assert_eq!(r_url, relay_url);
    assert_eq!(r_mailbox, mailbox_id);
    assert_eq!(r_token, write_token);
}
