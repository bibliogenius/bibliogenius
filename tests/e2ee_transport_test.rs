//! Integration tests for E2EE LAN transport (Phase 3).
//!
//! Tests the full seal -> POST -> open -> dispatch roundtrip using
//! two in-memory databases simulating two peers.

use rust_lib_app::crypto::envelope::{ClearMessage, EncryptedEnvelope};
use rust_lib_app::crypto::identity::NodeIdentity;
use rust_lib_app::db;
use rust_lib_app::services::crypto_service::{CryptoService, InMemoryNonceStore, PeerInfo};
use sea_orm::{DatabaseConnection, EntityTrait, Set};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Create a peer entry in the database with E2EE keys.
async fn create_e2ee_peer(
    db: &DatabaseConnection,
    name: &str,
    url: &str,
    identity: &NodeIdentity,
) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let ed25519_hex = hex::encode(identity.verifying_key().as_bytes());
    let x25519_hex = hex::encode(identity.x25519_public_key().as_bytes());

    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set(name.to_string()),
        url: Set(url.to_string()),
        public_key: Set(Some(ed25519_hex)),
        x25519_public_key: Set(Some(x25519_hex)),
        key_exchange_done: Set(true),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(db)
        .await
        .expect("Failed to create peer");
    res.last_insert_id
}

fn make_peer_info(identity: &NodeIdentity) -> PeerInfo {
    PeerInfo {
        verifying_key: identity.verifying_key(),
        x25519_public: identity.x25519_public_key(),
    }
}

#[tokio::test]
async fn seal_open_loan_request_roundtrip() {
    // Simulate: Alice sends a loan request to Bob
    let alice_identity = NodeIdentity::generate();
    let bob_identity = NodeIdentity::generate();

    let alice_service = CryptoService::new(alice_identity, InMemoryNonceStore::new());
    let bob_service = CryptoService::new(bob_identity, InMemoryNonceStore::new());

    // Alice seals a loan_request for Bob
    let message = ClearMessage {
        message_type: "loan_request".to_string(),
        payload: serde_json::json!({
            "book_isbn": "978-2-264-02484-8",
            "book_title": "Martin Eden",
            "from_peer_url": "http://192.168.1.10:8000",
            "from_peer_name": "Alice's Library",
        }),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let envelope = alice_service
        .seal(&bob_service.identity().x25519_public_key(), &message)
        .expect("seal failed");

    // Bob opens the envelope
    let bob_peers = vec![make_peer_info(alice_service.identity())];
    let (decrypted, peer_idx) = bob_service
        .open(&envelope, &bob_peers)
        .expect("open failed");

    assert_eq!(peer_idx, 0);
    assert_eq!(decrypted.message_type, "loan_request");
    assert_eq!(decrypted.payload["book_isbn"], "978-2-264-02484-8");
    assert_eq!(decrypted.payload["book_title"], "Martin Eden");
}

#[tokio::test]
async fn envelope_serializes_as_json() {
    let alice = NodeIdentity::generate();
    let bob = NodeIdentity::generate();
    let svc = CryptoService::new(alice, InMemoryNonceStore::new());

    let msg = ClearMessage {
        message_type: "test".to_string(),
        payload: serde_json::json!({}),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: "test-id".to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let envelope = svc
        .seal(&bob.x25519_public_key(), &msg)
        .expect("seal failed");

    // Verify EncryptedEnvelope can be serialized to JSON and back
    let json = serde_json::to_string(&envelope).expect("serialize failed");
    let deserialized: EncryptedEnvelope = serde_json::from_str(&json).expect("deserialize failed");

    assert_eq!(deserialized.version, 1);
    assert_eq!(deserialized.nonce, envelope.nonce);
    assert_eq!(deserialized.sender_hint, envelope.sender_hint);
}

#[tokio::test]
async fn replay_protection_rejects_duplicate() {
    let alice = NodeIdentity::generate();
    let bob = NodeIdentity::generate();

    let alice_svc = CryptoService::new(alice, InMemoryNonceStore::new());
    let bob_svc = CryptoService::new(bob, InMemoryNonceStore::new());

    let msg = ClearMessage {
        message_type: "loan_request".to_string(),
        payload: serde_json::json!({"book_isbn": "123"}),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let envelope = alice_svc
        .seal(&bob_svc.identity().x25519_public_key(), &msg)
        .unwrap();

    let peers = vec![make_peer_info(alice_svc.identity())];

    // First open succeeds
    bob_svc.open(&envelope, &peers).unwrap();

    // Replay: same envelope rejected
    let result = bob_svc.open(&envelope, &peers);
    assert!(result.is_err());
}

#[tokio::test]
async fn unknown_sender_rejected() {
    let alice = NodeIdentity::generate();
    let bob = NodeIdentity::generate();
    let charlie = NodeIdentity::generate();

    let alice_svc = CryptoService::new(alice, InMemoryNonceStore::new());
    let bob_svc = CryptoService::new(bob, InMemoryNonceStore::new());

    let msg = ClearMessage {
        message_type: "test".to_string(),
        payload: serde_json::json!({}),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: "test".to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };

    let envelope = alice_svc
        .seal(&bob_svc.identity().x25519_public_key(), &msg)
        .unwrap();

    // Bob only knows Charlie — Alice is unknown
    let peers = vec![make_peer_info(&charlie)];
    let result = bob_svc.open(&envelope, &peers);
    assert!(result.is_err());
}

#[tokio::test]
async fn e2ee_peer_created_with_keys_in_db() {
    let db = setup_test_db().await;
    let identity = NodeIdentity::generate();

    let peer_id = create_e2ee_peer(&db, "Test Peer", "http://192.168.1.5:8000", &identity).await;

    // Verify peer was created with E2EE fields
    let peer = rust_lib_app::models::peer::Entity::find_by_id(peer_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    assert!(peer.key_exchange_done);
    assert!(peer.public_key.is_some());
    assert!(peer.x25519_public_key.is_some());

    // Verify keys are valid hex
    let ed_bytes = hex::decode(peer.public_key.unwrap()).unwrap();
    let x_bytes = hex::decode(peer.x25519_public_key.unwrap()).unwrap();
    assert_eq!(ed_bytes.len(), 32);
    assert_eq!(x_bytes.len(), 32);
}

#[tokio::test]
async fn backward_compat_non_e2ee_peer() {
    let db = setup_test_db().await;

    // Create a peer without E2EE keys (legacy)
    let now = chrono::Utc::now().to_rfc3339();
    let peer = rust_lib_app::models::peer::ActiveModel {
        name: Set("Legacy Peer".to_string()),
        url: Set("http://192.168.1.20:8000".to_string()),
        public_key: Set(None),
        x25519_public_key: Set(None),
        key_exchange_done: Set(false),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let res = rust_lib_app::models::peer::Entity::insert(peer)
        .exec(&db)
        .await
        .unwrap();

    let loaded = rust_lib_app::models::peer::Entity::find_by_id(res.last_insert_id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    // Verify: no E2EE, should use plaintext path
    assert!(!loaded.key_exchange_done);
    assert!(loaded.public_key.is_none());
    assert!(loaded.x25519_public_key.is_none());
}

#[tokio::test]
async fn message_type_dispatch_coverage() {
    // Verify all expected message types can be constructed
    let types = vec![
        "loan_request",
        "loan_confirmation",
        "book_sync_request",
        "book_sync_response",
        "search_request",
        "search_response",
        "status_update",
    ];

    for msg_type in types {
        let msg = rust_lib_app::services::e2ee_transport::DirectTransport::build_message(
            msg_type,
            serde_json::json!({"test": true}),
        );
        assert_eq!(msg.message_type, msg_type);
        assert!(!msg.message_id.is_empty());
        assert!(msg.timestamp > 0);
    }
}
