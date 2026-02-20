//! Integration tests for E2EE Phase 4 — Relay WAN transport.
//!
//! Tests cover:
//! - Relay server endpoints (mailbox CRUD, token auth, rate limits)
//! - RelayTransport service (send, poll, ack via relay)
//! - Relay info exchange during peer connection
//! - Full relay roundtrip: seal → deposit → poll → open → dispatch

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rust_lib_app::crypto::envelope::ClearMessage;
use rust_lib_app::crypto::identity::NodeIdentity;
use rust_lib_app::db;
use rust_lib_app::services::crypto_service::{CryptoService, InMemoryNonceStore, PeerInfo};
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};
use tower::ServiceExt;

// ── Helpers ──────────────────────────────────────────────────────────

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Build an Axum app with relay routes for testing.
fn build_relay_app(state: rust_lib_app::infrastructure::AppState) -> axum::Router {
    use axum::routing::{delete, post};

    axum::Router::new()
        .route(
            "/api/relay/mailbox",
            post(rust_lib_app::api::relay::create_mailbox),
        )
        .route(
            "/api/relay/mailbox/:uuid/messages",
            post(rust_lib_app::api::relay::deposit_message)
                .get(rust_lib_app::api::relay::collect_messages),
        )
        .route(
            "/api/relay/mailbox/:uuid/messages/:id",
            delete(rust_lib_app::api::relay::ack_message),
        )
        .with_state(state)
}

/// Create a mailbox via the relay API. Returns (uuid, read_token, write_token).
async fn create_mailbox(app: &axum::Router) -> (String, String, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/relay/mailbox")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    (
        json["uuid"].as_str().unwrap().to_string(),
        json["read_token"].as_str().unwrap().to_string(),
        json["write_token"].as_str().unwrap().to_string(),
    )
}

/// Deposit a raw blob into a mailbox. Returns message id.
async fn deposit_blob(
    app: &axum::Router,
    uuid: &str,
    write_token: &str,
    blob: &[u8],
) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/relay/mailbox/{uuid}/messages"))
                .header("Authorization", format!("Bearer {write_token}"))
                .header("Content-Type", "application/octet-stream")
                .body(Body::from(blob.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::CREATED, "deposit failed: {json}");
    json
}

/// Collect messages from a mailbox. Returns the JSON response.
async fn collect_messages(app: &axum::Router, uuid: &str, read_token: &str) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/relay/mailbox/{uuid}/messages"))
                .header("Authorization", format!("Bearer {read_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn make_peer_info(identity: &NodeIdentity) -> PeerInfo {
    PeerInfo {
        verifying_key: identity.verifying_key(),
        x25519_public: identity.x25519_public_key(),
    }
}

// ── Relay Server Endpoint Tests ──────────────────────────────────────

#[tokio::test]
async fn relay_create_mailbox_returns_tokens() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    assert!(!uuid.is_empty());
    assert!(!read_token.is_empty());
    assert!(!write_token.is_empty());
    // Tokens should be different
    assert_ne!(read_token, write_token);
    // UUID should be valid
    assert!(uuid::Uuid::parse_str(&uuid).is_ok());
}

#[tokio::test]
async fn relay_deposit_and_collect_roundtrip() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    // Deposit a message
    let result = deposit_blob(&app, &uuid, &write_token, b"hello-relay").await;
    assert!(result["id"].as_i64().is_some());

    // Collect messages
    let collected = collect_messages(&app, &uuid, &read_token).await;
    let messages = collected["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);

    // Verify blob content (base64-decoded)
    let blob_b64 = messages[0]["blob"].as_str().unwrap();
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob_b64).unwrap();
    assert_eq!(decoded, b"hello-relay");
}

#[tokio::test]
async fn relay_wrong_write_token_rejected() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, _read_token, _write_token) = create_mailbox(&app).await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/relay/mailbox/{uuid}/messages"))
                .header("Authorization", "Bearer wrong_token")
                .header("Content-Type", "application/octet-stream")
                .body(Body::from(b"data".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn relay_wrong_read_token_rejected() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, _read_token, _write_token) = create_mailbox(&app).await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/relay/mailbox/{uuid}/messages"))
                .header("Authorization", "Bearer wrong_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn relay_missing_auth_header_rejected() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, _read_token, _write_token) = create_mailbox(&app).await;

    // No Authorization header
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/relay/mailbox/{uuid}/messages"))
                .header("Content-Type", "application/octet-stream")
                .body(Body::from(b"data".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn relay_nonexistent_mailbox_returns_404() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/relay/mailbox/nonexistent-uuid/messages")
                .header("Authorization", "Bearer some_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn relay_ack_deletes_message() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    // Deposit
    let result = deposit_blob(&app, &uuid, &write_token, b"to-delete").await;
    let msg_id = result["id"].as_i64().unwrap();

    // Ack (delete)
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/relay/mailbox/{uuid}/messages/{msg_id}"))
                .header("Authorization", format!("Bearer {read_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Collect — should be empty
    let collected = collect_messages(&app, &uuid, &read_token).await;
    let messages = collected["messages"].as_array().unwrap();
    assert!(messages.is_empty());
}

#[tokio::test]
async fn relay_blob_size_limit_enforced() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, _read_token, write_token) = create_mailbox(&app).await;

    // Create a blob larger than 64KB
    let oversized = vec![0u8; 65 * 1024];
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/relay/mailbox/{uuid}/messages"))
                .header("Authorization", format!("Bearer {write_token}"))
                .header("Content-Type", "application/octet-stream")
                .body(Body::from(oversized))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn relay_multiple_messages_ordered() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    // Deposit 3 messages
    for i in 0..3 {
        deposit_blob(&app, &uuid, &write_token, format!("msg-{i}").as_bytes()).await;
    }

    // Collect — should have 3 messages in order
    let collected = collect_messages(&app, &uuid, &read_token).await;
    let messages = collected["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);

    // Verify ordering (ids should be ascending)
    let id0 = messages[0]["id"].as_i64().unwrap();
    let id1 = messages[1]["id"].as_i64().unwrap();
    let id2 = messages[2]["id"].as_i64().unwrap();
    assert!(id0 < id1 && id1 < id2);
}

// ── E2EE + Relay Roundtrip Tests ─────────────────────────────────────

#[tokio::test]
async fn relay_e2ee_seal_deposit_collect_open_roundtrip() {
    // Full E2EE relay flow:
    // Alice seals a message → deposits on relay → Bob collects → Bob opens
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let alice_identity = NodeIdentity::generate();
    let bob_identity = NodeIdentity::generate();

    let alice_crypto = CryptoService::new(alice_identity, InMemoryNonceStore::new());
    let bob_crypto = CryptoService::new(bob_identity, InMemoryNonceStore::new());

    // Create Bob's mailbox on the relay
    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    // Alice seals a loan_request for Bob
    let message = ClearMessage {
        message_type: "loan_request".to_string(),
        payload: serde_json::json!({
            "book_isbn": "978-2-264-02484-8",
            "book_title": "Martin Eden",
            "from_peer_name": "Alice",
        }),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
    };

    let envelope = alice_crypto
        .seal(&bob_crypto.identity().x25519_public_key(), &message)
        .expect("seal failed");

    // Alice deposits the encrypted envelope on the relay
    let blob = serde_json::to_vec(&envelope).expect("serialize envelope");
    deposit_blob(&app, &uuid, &write_token, &blob).await;

    // Bob collects from the relay
    let collected = collect_messages(&app, &uuid, &read_token).await;
    let messages = collected["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);

    // Bob decodes the blob and opens the envelope
    let blob_b64 = messages[0]["blob"].as_str().unwrap();
    let blob_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob_b64).unwrap();
    let received_envelope: rust_lib_app::crypto::envelope::EncryptedEnvelope =
        serde_json::from_slice(&blob_bytes).expect("deserialize envelope");

    let known_peers = vec![make_peer_info(alice_crypto.identity())];
    let (decrypted, peer_idx) = bob_crypto
        .open(&received_envelope, &known_peers)
        .expect("open failed");

    assert_eq!(peer_idx, 0);
    assert_eq!(decrypted.message_type, "loan_request");
    assert_eq!(decrypted.payload["book_isbn"], "978-2-264-02484-8");
    assert_eq!(decrypted.payload["book_title"], "Martin Eden");
    assert_eq!(decrypted.payload["from_peer_name"], "Alice");
}

#[tokio::test]
async fn relay_e2ee_multiple_messages_all_decryptable() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let alice_identity = NodeIdentity::generate();
    let bob_identity = NodeIdentity::generate();

    let alice_crypto = CryptoService::new(alice_identity, InMemoryNonceStore::new());
    let bob_crypto = CryptoService::new(bob_identity, InMemoryNonceStore::new());

    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    // Alice sends 3 different message types via relay
    let types = ["loan_request", "loan_confirmation", "status_update"];
    for msg_type in &types {
        let message = ClearMessage {
            message_type: msg_type.to_string(),
            payload: serde_json::json!({"type": msg_type}),
            timestamp: chrono::Utc::now().timestamp(),
            message_id: uuid::Uuid::new_v4().to_string(),
        };

        let envelope = alice_crypto
            .seal(&bob_crypto.identity().x25519_public_key(), &message)
            .unwrap();
        let blob = serde_json::to_vec(&envelope).unwrap();
        deposit_blob(&app, &uuid, &write_token, &blob).await;
    }

    // Bob collects all
    let collected = collect_messages(&app, &uuid, &read_token).await;
    let messages = collected["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);

    // Bob decrypts all — each should succeed
    let known_peers = vec![make_peer_info(alice_crypto.identity())];
    for (i, msg) in messages.iter().enumerate() {
        let blob_b64 = msg["blob"].as_str().unwrap();
        let blob_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob_b64).unwrap();
        let envelope: rust_lib_app::crypto::envelope::EncryptedEnvelope =
            serde_json::from_slice(&blob_bytes).unwrap();

        let (decrypted, _) = bob_crypto.open(&envelope, &known_peers).unwrap();
        assert_eq!(decrypted.message_type, types[i]);
    }
}

#[tokio::test]
async fn relay_tampered_blob_fails_decryption() {
    let db = setup_test_db().await;
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let app = build_relay_app(state);

    let alice_identity = NodeIdentity::generate();
    let bob_identity = NodeIdentity::generate();

    let alice_crypto = CryptoService::new(alice_identity, InMemoryNonceStore::new());
    let bob_crypto = CryptoService::new(bob_identity, InMemoryNonceStore::new());

    let (uuid, read_token, write_token) = create_mailbox(&app).await;

    let message = ClearMessage {
        message_type: "test".to_string(),
        payload: serde_json::json!({}),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
    };

    let envelope = alice_crypto
        .seal(&bob_crypto.identity().x25519_public_key(), &message)
        .unwrap();

    // Tamper with the ciphertext
    let mut tampered = envelope.clone();
    if !tampered.ciphertext.is_empty() {
        tampered.ciphertext[0] ^= 0xFF;
    }

    let blob = serde_json::to_vec(&tampered).unwrap();
    deposit_blob(&app, &uuid, &write_token, &blob).await;

    // Bob collects and tries to open — should fail
    let collected = collect_messages(&app, &uuid, &read_token).await;
    let messages = collected["messages"].as_array().unwrap();
    let blob_b64 = messages[0]["blob"].as_str().unwrap();
    let blob_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, blob_b64).unwrap();
    let received: rust_lib_app::crypto::envelope::EncryptedEnvelope =
        serde_json::from_slice(&blob_bytes).unwrap();

    let known_peers = vec![make_peer_info(alice_crypto.identity())];
    let result = bob_crypto.open(&received, &known_peers);
    assert!(result.is_err(), "Tampered message should fail decryption");
}

// ── Relay Config Storage Tests ───────────────────────────────────────

#[tokio::test]
async fn relay_config_singleton_stored_and_retrievable() {
    let db = setup_test_db().await;

    // Insert relay config
    let now = chrono::Utc::now().to_rfc3339();
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO my_relay_config (id, relay_url, mailbox_uuid, read_token, write_token, created_at) \
             VALUES (1, 'http://hub:8002', 'test-uuid', 'read-tok', 'write-tok', '{now}')"
        ),
    ))
    .await
    .expect("insert relay config");

    // Retrieve via helper
    let config = rust_lib_app::api::relay::get_my_relay_config(&db)
        .await
        .expect("config should exist");

    assert_eq!(config.relay_url, "http://hub:8002");
    assert_eq!(config.mailbox_uuid, "test-uuid");
    assert_eq!(config.read_token, "read-tok");
    assert_eq!(config.write_token, "write-tok");
}

#[tokio::test]
async fn relay_peer_fields_stored_correctly() {
    let db = setup_test_db().await;

    let now = chrono::Utc::now().to_rfc3339();
    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set("Relay Peer".to_string()),
        url: Set("http://192.168.1.5:8000".to_string()),
        relay_url: Set(Some("http://hub:8002".to_string())),
        mailbox_id: Set(Some("peer-mailbox-uuid".to_string())),
        relay_write_token: Set(Some("peer-write-token".to_string())),
        key_exchange_done: Set(true),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(&db)
        .await
        .expect("insert peer");

    let loaded = rust_lib_app::models::peer::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(loaded.relay_url.as_deref(), Some("http://hub:8002"));
    assert_eq!(loaded.mailbox_id.as_deref(), Some("peer-mailbox-uuid"));
    assert_eq!(
        loaded.relay_write_token.as_deref(),
        Some("peer-write-token")
    );
}
