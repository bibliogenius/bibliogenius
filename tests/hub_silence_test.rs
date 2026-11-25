//! Nothing leaves for the hub while the gate is closed (ADR-073).
//!
//! The acceptance test the record asked for: an installation that never
//! agreed to register, WITH a paired peer that holds relay credentials,
//! changes a book, changes its profile, runs a poll cycle and receives a
//! connection request. The fake hub must not see a single request.
//!
//! Each scenario has its open-gate twin, so a scenario that sends nothing
//! for another reason (a missing key, a cooldown) cannot pass for silence.
//!
//! One process for this file, one gate: every test is serial.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rust_lib_app::api;
use rust_lib_app::crypto::identity::NodeIdentity;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{peer, relay_config};
use rust_lib_app::services::account_sync_client::{AccountSyncClient, AccountSyncError};
use rust_lib_app::services::catalog_notification::schedule_catalog_changed_notification;
use rust_lib_app::services::e2ee_transport::E2eeTransportError;
use rust_lib_app::services::hub_directory_service::{HubDirectoryError, HubDirectoryService};
use rust_lib_app::services::hub_gate;
use rust_lib_app::services::identity_service::IdentityService;
use rust_lib_app::services::nudge_events::NudgeSource;
use rust_lib_app::services::profile_notification::schedule_profile_changed_notification;
use rust_lib_app::services::relay_poller::{poll_once, recreate_mailbox};
use rust_lib_app::services::relay_transport::RelayTransport;
use sea_orm::{ActiveModelTrait, Set};
use serial_test::serial;
use tower::util::ServiceExt;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Long enough for the spawned notification tasks to reach the transport.
const BACKGROUND_SENDS: Duration = Duration::from_millis(600);

/// A hub that answers 200 to everything and remembers what it was asked.
async fn fake_hub() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "messages": [] })),
        )
        .mount(&server)
        .await;
    // SAFETY: every test of this file is serial.
    unsafe { std::env::set_var("HUB_URL", server.uri()) };
    server
}

async fn requests_seen(hub: &MockServer) -> Vec<String> {
    hub.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

/// A library with an identity, its own relay mailbox on `hub`, and one
/// accepted peer whose relay credentials point to `hub` too.
async fn library_with_a_peer(hub: &MockServer) -> AppState {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let identity_service = IdentityService::new(db.clone());
    identity_service
        .init(&uuid::Uuid::new_v4().to_string())
        .await
        .expect("init identity");
    let state = AppState::with_identity_service(db.clone(), Arc::new(identity_service));

    relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(hub.uri()),
        mailbox_uuid: Set(uuid::Uuid::new_v4().to_string()),
        read_token: Set("read-token".to_string()),
        write_token: Set("write-token".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    }
    .insert(&db)
    .await
    .expect("insert relay config");

    // The peer's keys only need to be well formed: borrow this library's.
    let identity: &NodeIdentity = state.crypto_service().expect("crypto").identity();
    let now = chrono::Utc::now().to_rfc3339();
    peer::ActiveModel {
        name: Set("Peer".to_string()),
        url: Set("http://192.168.1.50:8000".to_string()),
        library_uuid: Set(Some(uuid::Uuid::new_v4().to_string())),
        public_key: Set(Some(hex::encode(identity.verifying_key().to_bytes()))),
        x25519_public_key: Set(Some(hex::encode(identity.x25519_public_key().as_bytes()))),
        key_exchange_done: Set(true),
        relay_url: Set(Some(hub.uri())),
        mailbox_id: Set(Some(uuid::Uuid::new_v4().to_string())),
        relay_write_token: Set(Some("peer-write-token".to_string())),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert peer");

    state
}

/// What the core does on its own once a peer exists: a book change, a profile
/// change and a poll cycle.
async fn live_one_session(state: &AppState) {
    schedule_catalog_changed_notification(state.clone());
    schedule_profile_changed_notification(state.clone(), vec!["name".to_string()]);
    let _ = poll_once(state, NudgeSource::Polling).await;
    tokio::time::sleep(BACKGROUND_SENDS).await;
}

async fn receive_a_connection_request(state: &AppState) -> StatusCode {
    let app = axum::Router::new()
        .route(
            "/peers/incoming",
            axum::routing::post(api::peer::receive_connection_request),
        )
        .with_state(state.clone());
    let body = serde_json::json!({
        "name": "Visitor",
        "url": "http://192.168.1.60:8000",
    });
    let request = Request::builder()
        .method("POST")
        .uri("/peers/incoming")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    app.oneshot(request).await.expect("response").status()
}

#[tokio::test]
#[serial]
async fn a_closed_gate_sends_nothing_with_a_peer_a_book_change_and_a_connection_request() {
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    hub_gate::set_open(false);

    live_one_session(&state).await;
    let status = receive_a_connection_request(&state).await;

    assert!(
        status.is_success(),
        "the connection request is still handled locally, got {status}"
    );
    let seen = requests_seen(&hub).await;
    assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
}

#[tokio::test]
#[serial]
async fn the_same_session_reaches_the_hub_once_the_gate_is_open() {
    // The cooldown of the notifications is process-wide: let the closed-gate
    // test's slot expire, whatever the order the two tests ran in.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    hub_gate::set_open(true);

    live_one_session(&state).await;
    hub_gate::set_open(false);

    let seen = requests_seen(&hub).await;
    let deposits = seen.iter().filter(|r| r.starts_with("POST ")).count();
    let reads = seen.iter().filter(|r| r.starts_with("GET ")).count();
    assert!(
        deposits >= 2,
        "catalog and profile deposits expected: {seen:?}"
    );
    assert!(reads >= 1, "a mailbox read expected: {seen:?}");
}

#[tokio::test]
#[serial]
async fn a_connection_request_is_never_forwarded_to_the_hub_even_with_the_gate_open() {
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    hub_gate::set_open(true);

    let status = receive_a_connection_request(&state).await;
    hub_gate::set_open(false);

    assert!(status.is_success(), "got {status}");
    let seen = requests_seen(&hub).await;
    assert!(
        !seen.iter().any(|r| r.contains("/api/peers")),
        "the requester's name and address went to the hub: {seen:?}"
    );
}

#[tokio::test]
#[serial]
async fn the_relay_transport_refuses_every_request_when_the_gate_is_closed() {
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    hub_gate::set_open(false);
    let crypto = state.crypto_service().expect("crypto").clone();
    let key = crypto.identity().x25519_public_key();
    let relay = RelayTransport::new(Some(crypto));
    let message = rust_lib_app::crypto::envelope::ClearMessage {
        message_type: "catalog_changed".to_string(),
        payload: serde_json::json!({}),
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    };
    let url = hub.uri();

    let silenced = |result: Result<(), E2eeTransportError>| {
        matches!(result, Err(E2eeTransportError::HubSilenced))
    };
    assert!(silenced(
        relay.send(&url, "box", "token", &key, &message).await
    ));
    assert!(silenced(
        relay
            .deposit_response(&url, "box", "token", &key, &message)
            .await
    ));
    assert!(silenced(
        relay.deposit_raw(&url, "box", "token", b"{}").await
    ));
    assert!(silenced(relay.ack(&url, "box", "token", 1).await));
    assert!(matches!(
        relay.poll(&url, "box", "token").await,
        Err(E2eeTransportError::HubSilenced)
    ));
    let client = reqwest::Client::new();
    assert!(matches!(
        RelayTransport::create_mailbox(&client, &url).await,
        Err(E2eeTransportError::HubSilenced)
    ));
    assert!(matches!(
        RelayTransport::delete_mailbox(&client, &url, "box", "token").await,
        Err(E2eeTransportError::HubSilenced)
    ));
    assert!(
        recreate_mailbox(state.db(), &url, "box").await.is_err(),
        "a mailbox must not be recreated behind a closed gate"
    );

    let seen = requests_seen(&hub).await;
    assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
}

#[tokio::test]
#[serial]
async fn the_directory_and_account_clients_refuse_when_the_gate_is_closed() {
    let hub = fake_hub().await;
    hub_gate::set_open(false);

    let directory = HubDirectoryService::new()
        .list_directory(10, 0, None, None, None)
        .await;
    assert!(
        matches!(directory, Err(HubDirectoryError::HubSilenced)),
        "got {directory:?}"
    );

    let account = AccountSyncClient::with_base_url(hub.uri())
        .bootstrap("reader@example.org")
        .await;
    assert!(
        matches!(account, Err(AccountSyncError::HubSilenced)),
        "the account client must refuse"
    );

    let seen = requests_seen(&hub).await;
    assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
}

#[tokio::test]
#[serial]
async fn the_address_of_the_hub_can_still_be_read_when_the_gate_is_closed() {
    let hub = fake_hub().await;
    hub_gate::set_open(false);

    // The app files its Keychain copies of the hub credentials under this
    // address: reading it sends nothing and must not depend on the gate.
    assert_eq!(
        rust_lib_app::api::frb::hub_effective_url(),
        Some(hub.uri().trim_end_matches('/').to_string())
    );
}

#[tokio::test]
#[serial]
async fn the_nudge_listener_does_not_connect_while_the_gate_is_closed() {
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    hub_gate::set_open(false);

    let listener = tokio::spawn(rust_lib_app::services::ws_nudge::start_ws_nudge(state));
    tokio::time::sleep(BACKGROUND_SENDS).await;
    listener.abort();

    let seen = requests_seen(&hub).await;
    assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
}

#[tokio::test]
#[serial]
async fn the_nudge_listener_connects_once_the_gate_opens() {
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    hub_gate::set_open(false);

    let listener = tokio::spawn(rust_lib_app::services::ws_nudge::start_ws_nudge(state));
    tokio::time::sleep(Duration::from_millis(200)).await;
    hub_gate::set_open(true);
    tokio::time::sleep(BACKGROUND_SENDS).await;
    hub_gate::set_open(false);
    listener.abort();

    let seen = requests_seen(&hub).await;
    assert!(
        seen.iter().any(|r| r == "GET /ws"),
        "the listener should have tried to connect: {seen:?}"
    );
}

#[test]
#[serial]
fn the_gate_starts_closed_and_follows_what_it_is_told() {
    hub_gate::set_open(false);
    assert_eq!(hub_gate::ensure_open(), Err(hub_gate::HubSilenced));
    hub_gate::set_open(true);
    assert_eq!(hub_gate::ensure_open(), Ok(()));
    hub_gate::set_open(false);
}

#[tokio::test]
#[serial]
async fn a_listener_is_woken_when_the_gate_closes() {
    hub_gate::set_open(true);
    let mut receiver = hub_gate::subscribe();
    let waiter = tokio::spawn(async move { hub_gate::closed(&mut receiver).await });
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "an open gate must not wake the wait");

    hub_gate::set_open(false);
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("closing the gate wakes the listener")
        .expect("the waiting task ends cleanly");
}

#[tokio::test]
#[serial]
async fn covers_are_not_addressed_on_the_hub_while_the_gate_is_closed() {
    use sea_orm::{ConnectionTrait, Statement};
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    let db = state.db();
    let now = chrono::Utc::now().to_rfc3339();
    // A registration left by an earlier build, on an installation that no
    // longer uses the hub.
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO hub_directory_config (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, created_at, updated_at) \
             VALUES (1, 'my-node-id', 'token', 0, 1, 'everyone', 0, '{now}', '{now}')"
        ),
    ))
    .await
    .expect("insert hub_directory_config");

    hub_gate::set_open(true);
    assert!(
        rust_lib_app::models::book::Book::hub_cover_prefix(db)
            .await
            .is_some(),
        "a registered library addresses its covers on the hub"
    );

    hub_gate::set_open(false);
    assert_eq!(
        rust_lib_app::models::book::Book::hub_cover_prefix(db).await,
        None,
        "peers must not be sent to the hub for a cover"
    );
}

#[tokio::test]
#[serial]
async fn a_photographed_cover_is_served_on_the_local_network_when_the_hub_is_not_used() {
    use rust_lib_app::models::book::{ActiveModel, Book};
    use sea_orm::{ActiveModelTrait, Set};
    let hub = fake_hub().await;
    let state = library_with_a_peer(&hub).await;
    let now = chrono::Utc::now().to_rfc3339();
    let stored = ActiveModel {
        title: Set("With a photographed cover".to_owned()),
        owned: Set(true),
        private: Set(false),
        cover_url: Set(Some("/var/mobile/covers/book.jpg".to_owned())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(state.db())
    .await
    .expect("insert book");
    let catalogue = || vec![Book::from(stored.clone())];

    // Two libraries that never registered: nothing can leave by the relay,
    // so a catalogue gives the address the peer resolves on the local
    // network, through its cover proxy.
    hub_gate::set_open(false);
    let mut books = catalogue();
    Book::rewrite_cover_urls_for_relay(&mut books, None, None);
    let address = books[0].cover_url.clone().expect("a cover address");
    assert!(
        address.starts_with(&format!("/api/books/{}/cover", stored.id)),
        "got {address}"
    );

    // With the hub in use and no address there for the cover, the answer
    // may travel by the relay to a peer with no route here: the cover is
    // dropped rather than sent as an address that peer cannot reach.
    hub_gate::set_open(true);
    let mut books = catalogue();
    Book::rewrite_cover_urls_for_relay(&mut books, None, None);
    assert_eq!(books[0].cover_url, None);
    hub_gate::set_open(false);
}

#[tokio::test]
#[serial]
async fn a_loan_never_carries_a_local_network_cover_address() {
    use rust_lib_app::models::book::Book;
    // The borrower stores this value in a book of its own library, where a
    // relative address would point at its own server: gate closed or not,
    // a photographed cover without a hub address is left out of a loan.
    for open in [false, true] {
        hub_gate::set_open(open);
        assert_eq!(
            Book::safe_cover_url_for_relay(
                Some("/var/mobile/covers/book.jpg"),
                "book-1",
                None,
                None,
                None
            ),
            None,
            "gate open: {open}"
        );
    }
    hub_gate::set_open(false);
}
