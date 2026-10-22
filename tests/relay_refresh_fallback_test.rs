//! Tests for relay credential refresh LAN -> Hub fallback.
//!
//! Covers:
//! 1. LAN unreachable + hub returns valid creds (same x25519) -> fallback works
//! 2. LAN unreachable + hub x25519 mismatch -> fallback rejected
//! 3. No library_uuid + LAN unreachable -> no fallback attempted
//! 4. LAN reachable and answering as the peer -> hub not called
//! 5. Hub confirmation of a peer's new keys (another published x25519)

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

/// Store `relay_url` as the hub holding the peer's mailbox.
async fn set_peer_relay(
    db: &DatabaseConnection,
    peer: &peer::Model,
    relay_url: &str,
) -> peer::Model {
    use sea_orm::ActiveModelTrait;
    let mut active: peer::ActiveModel = peer.clone().into();
    active.relay_url = Set(Some(relay_url.to_string()));
    active.update(db).await.expect("set relay")
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

    // LAN mock server answers as the stored peer, with valid relay config
    let lan = MockServer::start().await;
    let relay_url = "https://hub.example.com";
    let mailbox_id = "lan-mailbox-uuid";
    let write_token = "lan-write-tok";

    Mock::given(method("GET"))
        .and(path("/api/config"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 1,
            "library_name": "iPhone Library",
            "library_uuid": "peer-node-789",
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

/// The hub publishes another x25519 key for the peer's uuid: the peer has
/// new keys, confirmed by a source a LAN host cannot speak for.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn hub_confirms_new_keys_when_it_publishes_another_key() {
    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    let pinned = "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    let published = "ffeeddcc11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    Mock::given(method("GET"))
        .and(path("/api/directory/peer-node-new-keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hub_profile_json(
            "peer-node-new-keys",
            published,
            "https://hub.example.com",
            "mbx",
            "tok",
        )))
        .expect(1)
        .mount(&hub)
        .await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;
    let peer_model = insert_peer(
        &db,
        "Reinstalled",
        "http://192.168.1.99:8000",
        Some("peer-node-new-keys"),
        Some(pinned),
    )
    .await;
    // The peer's mailbox lives on our hub, with a trailing slash.
    let peer_model = set_peer_relay(&db, &peer_model, &format!("{}/", hub.uri())).await;

    assert!(rust_lib_app::api::peer::hub_confirms_new_keys(&db, &peer_model).await);
}

/// A peer whose mailbox is not on our hub may never have registered there,
/// and the first registrant of a node id owns it: whatever our hub
/// publishes under its uuid proves nothing, and the hub is not even asked.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_peer_away_from_our_hub_is_never_confirmed() {
    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    let pinned = "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    let published = "ffeeddcc11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    Mock::given(method("GET"))
        .and(path("/api/directory/peer-elsewhere"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hub_profile_json(
            "peer-elsewhere",
            published,
            "https://hub.example.com",
            "mbx",
            "tok",
        )))
        .expect(0)
        .mount(&hub)
        .await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;

    let no_relay = insert_peer(
        &db,
        "LAN only",
        "http://192.168.1.95:8000",
        Some("peer-elsewhere"),
        Some(pinned),
    )
    .await;
    assert!(!rust_lib_app::api::peer::hub_confirms_new_keys(&db, &no_relay).await);

    let other_hub = set_peer_relay(&db, &no_relay, "https://another-hub.example.org").await;
    assert!(!rust_lib_app::api::peer::hub_confirms_new_keys(&db, &other_hub).await);
}

/// A slow hub confirms nothing: the answer must reach the app before its
/// own timeout.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_slow_hub_confirms_nothing() {
    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    let pinned = "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    let published = "ffeeddcc11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    Mock::given(method("GET"))
        .and(path("/api/directory/peer-slow-hub"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(hub_profile_json(
                    "peer-slow-hub",
                    published,
                    "https://hub.example.com",
                    "mbx",
                    "tok",
                ))
                .set_delay(std::time::Duration::from_secs(4)),
        )
        .mount(&hub)
        .await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;
    let peer_model = insert_peer(
        &db,
        "Slow",
        "http://192.168.1.94:8000",
        Some("peer-slow-hub"),
        Some(pinned),
    )
    .await;
    let peer_model = set_peer_relay(&db, &peer_model, &hub.uri()).await;

    let started = std::time::Instant::now();
    assert!(!rust_lib_app::api::peer::hub_confirms_new_keys(&db, &peer_model).await);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
}

/// The hub still publishes the pinned key (in another case, too), answers
/// an error, or has no key: nothing is confirmed.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn hub_does_not_confirm_new_keys_otherwise() {
    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    let pinned = "aabbccdd11223344aabbccdd11223344aabbccdd11223344aabbccdd11223344";
    Mock::given(method("GET"))
        .and(path("/api/directory/peer-same-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(hub_profile_json(
            "peer-same-key",
            &pinned.to_uppercase(),
            "https://hub.example.com",
            "mbx",
            "tok",
        )))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/directory/peer-hub-error"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&hub)
        .await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    insert_hub_config(&db, "our-write-token").await;

    let same_key = insert_peer(
        &db,
        "Same key",
        "http://192.168.1.98:8000",
        Some("peer-same-key"),
        Some(pinned),
    )
    .await;
    let same_key = set_peer_relay(&db, &same_key, &hub.uri()).await;
    assert!(!rust_lib_app::api::peer::hub_confirms_new_keys(&db, &same_key).await);

    let hub_error = insert_peer(
        &db,
        "Hub error",
        "http://192.168.1.97:8000",
        Some("peer-hub-error"),
        Some(pinned),
    )
    .await;
    let hub_error = set_peer_relay(&db, &hub_error, &hub.uri()).await;
    assert!(!rust_lib_app::api::peer::hub_confirms_new_keys(&db, &hub_error).await);

    let no_pinned_key = insert_peer(
        &db,
        "No pinned key",
        "http://192.168.1.96:8000",
        Some("peer-same-key"),
        None,
    )
    .await;
    let no_pinned_key = set_peer_relay(&db, &no_pinned_key, &hub.uri()).await;
    assert!(!rust_lib_app::api::peer::hub_confirms_new_keys(&db, &no_pinned_key).await);
}
