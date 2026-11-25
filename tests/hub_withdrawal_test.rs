//! Withdrawing the hub registration (ADR-073 D4).
//!
//! The device goes silent first, then asks the hub to delete what it holds:
//! the profile and the relay mailbox. Those two requests are the only ones
//! that leave while the gate is closed, and only from the withdrawal itself.
//! After them, nothing: a book change, a poll cycle and an incoming
//! connection request send nothing more. A hub that does not answer changes
//! nothing to the silence; the deletion is left to be tried again.
//!
//! One process for this file, one gate: every test is serial.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rust_lib_app::api;
use rust_lib_app::crypto::account_keys::AccountKeyBundle;
use rust_lib_app::crypto::identity::NodeIdentity;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{peer, relay_config};
use rust_lib_app::services::account_session_service;
use rust_lib_app::services::catalog_notification::schedule_catalog_changed_notification;
use rust_lib_app::services::e2ee_transport::E2eeTransportError;
use rust_lib_app::services::hub_catalog_key_store::CatalogKeyStore;
use rust_lib_app::services::hub_directory_service::HubDirectoryService;
use rust_lib_app::services::hub_gate;
use rust_lib_app::services::hub_withdrawal::{
    HubRegistration, WithdrawalError, WithdrawalOutcome, withdraw,
};
use rust_lib_app::services::identity_service::IdentityService;
use rust_lib_app::services::nudge_events::NudgeSource;
use rust_lib_app::services::profile_notification::schedule_profile_changed_notification;
use rust_lib_app::services::relay_poller::poll_once;
use rust_lib_app::services::relay_transport::RelayTransport;
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, Set, Statement};
use serial_test::serial;
use tower::util::ServiceExt;
use wiremock::matchers::{any, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Long enough for the spawned notification tasks to reach the transport.
const BACKGROUND_SENDS: Duration = Duration::from_millis(600);

const NODE_ID: &str = "node-of-this-library";
const MAILBOX: &str = "0b6f1c1e-1111-4222-8333-444455556666";

/// A hub that answers `status` to everything and remembers what it was asked.
async fn fake_hub(status: u16) -> MockServer {
    let server = MockServer::start().await;
    answer_with(&server, status, None).await;
    // SAFETY: every test of this file is serial.
    unsafe { std::env::set_var("HUB_URL", server.uri()) };
    server
}

async fn answer_with(server: &MockServer, status: u16, delay: Option<Duration>) {
    server.reset().await;
    let mut response =
        ResponseTemplate::new(status).set_body_json(serde_json::json!({ "messages": [] }));
    if let Some(delay) = delay {
        response = response.set_delay(delay);
    }
    Mock::given(any())
        .respond_with(response)
        .mount(server)
        .await;
}

async fn requests_seen(hub: &MockServer) -> Vec<String> {
    hub.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

fn the_two_deletions() -> Vec<String> {
    vec![
        "DELETE /api/directory/profile".to_string(),
        format!("DELETE /api/relay/mailbox/{MAILBOX}"),
    ]
}

/// A library registered on `hub`: its registration row, its catalog key, its
/// relay mailbox, and one accepted peer whose relay credentials point to
/// `hub` too.
async fn registered_library(hub_url: &str) -> AppState {
    let db = db::init_db("sqlite::memory:").await.expect("init db");
    let identity_service = IdentityService::new(db.clone());
    identity_service
        .init(&uuid::Uuid::new_v4().to_string())
        .await
        .expect("init identity");
    let state = AppState::with_identity_service(db.clone(), Arc::new(identity_service));

    let now = chrono::Utc::now().to_rfc3339();
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO hub_directory_config
                 (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, recovery_code, created_at, updated_at)
             VALUES (1, '{NODE_ID}', 'directory-token', 0, 1, 'everyone', 1, 'rc-1', '{now}', '{now}')"
        ),
    ))
    .await
    .expect("insert registration");
    CatalogKeyStore::shared()
        .load_or_create(&db, "11111111-2222-3333-4444-555555555555")
        .await
        .expect("catalog key");

    relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(hub_url.to_string()),
        mailbox_uuid: Set(MAILBOX.to_string()),
        read_token: Set("read-token".to_string()),
        write_token: Set("write-token".to_string()),
        created_at: Set(now.clone()),
    }
    .insert(&db)
    .await
    .expect("insert relay config");

    // The peer's keys only need to be well formed: borrow this library's.
    let identity: &NodeIdentity = state.crypto_service().expect("crypto").identity();
    peer::ActiveModel {
        name: Set("Peer".to_string()),
        url: Set("http://192.168.1.50:8000".to_string()),
        library_uuid: Set(Some(uuid::Uuid::new_v4().to_string())),
        public_key: Set(Some(hex::encode(identity.verifying_key().to_bytes()))),
        x25519_public_key: Set(Some(hex::encode(identity.x25519_public_key().as_bytes()))),
        key_exchange_done: Set(true),
        relay_url: Set(Some(hub_url.to_string())),
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

/// What the caller names: the registration it read on this device before
/// asking for the withdrawal.
fn this_registration() -> HubRegistration {
    HubRegistration {
        node_id: Some(NODE_ID.to_string()),
        mailbox_uuid: Some(MAILBOX.to_string()),
    }
}

async fn run_withdrawal(
    state: &AppState,
    named: HubRegistration,
) -> Result<WithdrawalOutcome, WithdrawalError> {
    withdraw(
        state.db(),
        Some(state.relay_poll_lock().as_ref()),
        &HubDirectoryService::new(),
        CatalogKeyStore::shared(),
        &named,
    )
    .await
}

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        db.get_database_backend(),
        sql.to_owned(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get("", "n")
    .expect("n")
}

async fn registration_rows(db: &DatabaseConnection) -> i64 {
    count(db, "SELECT COUNT(*) AS n FROM hub_directory_config").await
}

async fn relay_rows(db: &DatabaseConnection) -> i64 {
    count(db, "SELECT COUNT(*) AS n FROM my_relay_config").await
}

async fn live_catalog_keys(db: &DatabaseConnection) -> i64 {
    count(
        db,
        "SELECT COUNT(*) AS n FROM crypto_keys WHERE key_type = 'catalog' AND revoked_at IS NULL",
    )
    .await
}

async fn peers_with_relay_credentials(db: &DatabaseConnection) -> i64 {
    count(
        db,
        "SELECT COUNT(*) AS n FROM peers WHERE relay_write_token IS NOT NULL",
    )
    .await
}

/// What the core does on its own once a peer exists: a book change, a profile
/// change, a poll cycle, and an incoming connection request.
async fn live_one_session(state: &AppState) {
    schedule_catalog_changed_notification(state.clone());
    schedule_profile_changed_notification(state.clone(), vec!["name".to_string()]);
    let _ = poll_once(state, NudgeSource::Polling).await;
    let app = axum::Router::new()
        .route(
            "/peers/incoming",
            axum::routing::post(api::peer::receive_connection_request),
        )
        .with_state(state.clone());
    let body = serde_json::json!({ "name": "Visitor", "url": "http://192.168.1.60:8000" });
    let request = Request::builder()
        .method("POST")
        .uri("/peers/incoming")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let status: StatusCode = app.oneshot(request).await.expect("response").status();
    assert!(status.is_success(), "connection request: {status}");
    tokio::time::sleep(BACKGROUND_SENDS).await;
}

#[tokio::test]
#[serial]
async fn a_withdrawal_deletes_the_hub_side_then_nothing_more_is_sent() {
    let hub = fake_hub(204).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    assert_eq!(requests_seen(&hub).await, the_two_deletions());
    assert!(!hub_gate::is_open(), "the withdrawal closes the gate");
    assert!(hub_gate::refused(), "and it counts as a real no");

    let db = state.db();
    assert_eq!(registration_rows(db).await, 0);
    assert_eq!(relay_rows(db).await, 0);
    assert_eq!(live_catalog_keys(db).await, 0, "the catalog key is retired");
    assert_eq!(
        peers_with_relay_credentials(db).await,
        1,
        "the peers' relay credentials are kept, unused"
    );

    live_one_session(&state).await;
    assert_eq!(
        requests_seen(&hub).await,
        the_two_deletions(),
        "nothing may follow the two deletions"
    );
}

#[tokio::test]
#[serial]
async fn the_deletion_carries_the_right_tokens() {
    let hub = fake_hub(204).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    run_withdrawal(&state, this_registration())
        .await
        .expect("withdrawal");

    let requests = hub.received_requests().await.unwrap_or_default();
    let bearer = |index: usize| {
        requests[index]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    assert_eq!(bearer(0), "Bearer directory-token");
    assert_eq!(bearer(1), "Bearer read-token");
}

#[tokio::test]
#[serial]
async fn a_hub_that_fails_leaves_the_device_silent_and_the_deletion_to_retry() {
    let hub = fake_hub(503).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Pending));
    assert!(!hub_gate::is_open());
    let db = state.db();
    assert_eq!(
        registration_rows(db).await,
        1,
        "the token is kept for the retry"
    );
    assert_eq!(relay_rows(db).await, 1, "and so is the mailbox to delete");

    live_one_session(&state).await;
    assert_eq!(
        requests_seen(&hub).await,
        the_two_deletions(),
        "a pending deletion does not reopen anything"
    );

    // The hub is back: the same call finishes the job.
    answer_with(&hub, 204, None).await;
    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    assert_eq!(requests_seen(&hub).await, the_two_deletions());
    assert_eq!(registration_rows(db).await, 0);
    assert_eq!(relay_rows(db).await, 0);
}

#[tokio::test]
#[serial]
async fn a_hub_that_cannot_be_reached_leaves_the_device_silent() {
    // Nothing listens on this port.
    let dead_hub = "http://127.0.0.1:1";
    // SAFETY: every test of this file is serial.
    unsafe { std::env::set_var("HUB_URL", dead_hub) };
    let state = registered_library(dead_hub).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Pending));
    assert!(!hub_gate::is_open());
    assert!(hub_gate::refused());
    assert_eq!(registration_rows(state.db()).await, 1);
}

#[tokio::test]
#[serial]
async fn a_registration_the_hub_has_nothing_under_counts_as_deleted() {
    let hub = fake_hub(404).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    assert_eq!(registration_rows(state.db()).await, 0);
    assert_eq!(relay_rows(state.db()).await, 0);
}

#[tokio::test]
#[serial]
async fn a_refused_token_with_no_recovery_code_leaves_nothing_to_try() {
    // Nothing on this device can speak for a profile any more.
    let hub = fake_hub(401).await;
    let state = registered_library(&hub.uri()).await;
    state
        .db()
        .execute(Statement::from_string(
            state.db().get_database_backend(),
            "UPDATE hub_directory_config SET recovery_code = NULL".to_owned(),
        ))
        .await
        .expect("drop the recovery code");
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    assert_eq!(registration_rows(state.db()).await, 0);
}

#[tokio::test]
#[serial]
async fn a_refused_token_and_a_refused_recovery_code_are_not_a_deletion() {
    // The hub answers the same to a profile that is gone and to a recovery
    // code it rotated in an answer this device never received: the profile
    // may still stand. Never "done"; and never asked again, since the same
    // code would be refused again.
    let hub = fake_hub(401).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Unconfirmed));
    assert!(!hub_gate::is_open(), "silent all the same");
    assert_eq!(
        registration_rows(state.db()).await,
        0,
        "the refusal is final: nothing is kept to ask again"
    );
    assert_eq!(relay_rows(state.db()).await, 0);

    // And nothing is asked again.
    let asked = requests_seen(&hub).await.len();
    let outcome = run_withdrawal(&state, this_registration()).await;
    assert_eq!(outcome, Ok(WithdrawalOutcome::Done), "nothing left here");
    assert_eq!(requests_seen(&hub).await.len(), asked);
}

#[tokio::test]
#[serial]
async fn a_recovery_the_hub_fails_to_answer_is_tried_again_and_never_read_as_done() {
    let hub = fake_hub(204).await;
    let refuse_the_token = || {
        Mock::given(method("DELETE"))
            .and(path("/api/directory/profile"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(1)
    };
    refuse_the_token().mount(&hub).await;
    Mock::given(method("POST"))
        .and(path("/api/directory/recover"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&hub)
        .await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;
    assert_eq!(outcome, Ok(WithdrawalOutcome::Pending));
    assert_eq!(registration_rows(state.db()).await, 1);

    // The hub did rotate the code while its answer was lost: at the next
    // try it refuses the old one. Still not a deletion.
    answer_with(&hub, 204, None).await;
    refuse_the_token().mount(&hub).await;
    Mock::given(method("POST"))
        .and(path("/api/directory/recover"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(1)
        .mount(&hub)
        .await;

    let outcome = run_withdrawal(&state, this_registration()).await;
    assert_eq!(outcome, Ok(WithdrawalOutcome::Unconfirmed));
    assert_eq!(registration_rows(state.db()).await, 0);
}

#[test]
fn the_withdrawal_purges_through_the_shared_function() {
    // `forget_registration` also drops the covers this run uploaded and the
    // upload failures (unit-tested in the service); the bare
    // `purge_local_config` would leave them behind.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/hub_withdrawal.rs"),
    )
    .expect("read source");
    assert!(source.contains("hub.forget_registration("));
    assert!(!source.contains("purge_local_config("));
}

#[tokio::test]
#[serial]
async fn a_stale_token_is_renewed_with_the_recovery_code_before_deleting() {
    // The hub answers 401 to a token it does not know, whether the profile
    // is gone or the local token is simply out of date (a restored row).
    // Reading that 401 as "deleted" would leave the profile on the hub.
    let hub = fake_hub(204).await;
    Mock::given(method("DELETE"))
        .and(path("/api/directory/profile"))
        .and(header("authorization", "Bearer directory-token"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(1)
        .mount(&hub)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/directory/recover"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "node_id": NODE_ID,
            "display_name": "Library",
            "book_count": 0,
            "requires_approval": true,
            "write_token": "fresh-token",
            "recovery_code": "rc-2",
        })))
        .with_priority(1)
        .mount(&hub)
        .await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    let requests = hub.received_requests().await.unwrap_or_default();
    let seen: Vec<String> = requests
        .iter()
        .map(|r| {
            let bearer = r
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-");
            format!("{} {} {bearer}", r.method, r.url.path())
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            "DELETE /api/directory/profile Bearer directory-token".to_string(),
            "POST /api/directory/recover -".to_string(),
            "DELETE /api/directory/profile Bearer fresh-token".to_string(),
            format!("DELETE /api/relay/mailbox/{MAILBOX} Bearer read-token"),
        ]
    );
    assert_eq!(registration_rows(state.db()).await, 0);
}

#[tokio::test]
#[serial]
async fn a_profile_deleted_and_a_mailbox_unanswered_keep_only_the_mailbox_to_retry() {
    let hub = fake_hub(204).await;
    Mock::given(method("DELETE"))
        .and(path(format!("/api/relay/mailbox/{MAILBOX}")))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&hub)
        .await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Pending));
    assert_eq!(
        registration_rows(state.db()).await,
        0,
        "the profile is gone: its row and key go"
    );
    assert_eq!(live_catalog_keys(state.db()).await, 0);
    assert_eq!(
        relay_rows(state.db()).await,
        1,
        "the mailbox is still to delete"
    );

    // Next try: the profile is not asked again, only the mailbox.
    answer_with(&hub, 204, None).await;
    let outcome = run_withdrawal(&state, this_registration()).await;

    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    assert_eq!(
        requests_seen(&hub).await,
        vec![format!("DELETE /api/relay/mailbox/{MAILBOX}")]
    );
    assert_eq!(relay_rows(state.db()).await, 0);
}

#[tokio::test]
#[serial]
async fn rows_the_caller_did_not_name_are_dropped_without_a_request() {
    // A restored backup can bring back the registration of another library:
    // its token must never be used to delete anything on the hub.
    for named in [
        HubRegistration::default(),
        HubRegistration {
            node_id: Some("another-library".to_string()),
            mailbox_uuid: Some("another-mailbox".to_string()),
        },
    ] {
        let hub = fake_hub(204).await;
        let state = registered_library(&hub.uri()).await;
        hub_gate::set_open(true);

        let outcome = run_withdrawal(&state, named).await;

        assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
        let seen = requests_seen(&hub).await;
        assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
        assert_eq!(registration_rows(state.db()).await, 0);
        assert_eq!(relay_rows(state.db()).await, 0);
        assert!(!hub_gate::is_open());
    }
}

#[tokio::test]
#[serial]
async fn the_pass_of_the_withdrawal_lets_nobody_else_through() {
    let hub = fake_hub(204).await;
    answer_with(&hub, 204, Some(Duration::from_millis(400))).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);

    let withdrawing = {
        let state = state.clone();
        tokio::spawn(async move { run_withdrawal(&state, this_registration()).await })
    };
    // The withdrawal is waiting for the hub's first answer.
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert!(
        !hub_gate::is_open(),
        "the gate is closed before the deletion"
    );
    let client = reqwest::Client::new();
    assert!(
        matches!(
            RelayTransport::create_mailbox(&client, &hub.uri()).await,
            Err(E2eeTransportError::HubSilenced)
        ),
        "another sender must be refused while the withdrawal runs"
    );
    live_one_session(&state).await;

    let outcome = withdrawing.await.expect("join");
    assert_eq!(outcome, Ok(WithdrawalOutcome::Done));
    assert_eq!(requests_seen(&hub).await, the_two_deletions());
}

#[tokio::test]
#[serial]
async fn a_withdrawal_waits_for_the_poll_cycle_in_progress() {
    // The withdrawal reads the relay row only once the poll cycle in
    // progress is over, never in the middle of a mailbox recreation. (A
    // mailbox that cycle did recreate is not the one the caller named: its
    // row is dropped without a request, and the hub's own expiry removes
    // it. Rare, and nobody knows that mailbox.)
    let hub = fake_hub(204).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);
    let poll_lock = state.relay_poll_lock().clone();
    let cycle = poll_lock.lock().await;

    let withdrawing = {
        let state = state.clone();
        tokio::spawn(async move { run_withdrawal(&state, this_registration()).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(!hub_gate::is_open(), "silent first, even while waiting");
    assert!(
        requests_seen(&hub).await.is_empty(),
        "no deletion while a poll cycle runs"
    );

    drop(cycle);
    assert_eq!(
        withdrawing.await.expect("join"),
        Ok(WithdrawalOutcome::Done)
    );
    assert_eq!(requests_seen(&hub).await, the_two_deletions());
}

#[tokio::test]
#[serial]
async fn a_device_signed_in_to_an_account_is_refused_and_nothing_changes() {
    let hub = fake_hub(204).await;
    let state = registered_library(&hub.uri()).await;
    hub_gate::set_open(true);
    let bundle = AccountKeyBundle::generate();
    account_session_service::persist(
        state.db(),
        "lib-uuid",
        "acct-1",
        "r@e.org",
        "dev-a",
        &bundle,
    )
    .await
    .expect("account session");

    let outcome = run_withdrawal(&state, this_registration()).await;
    let still_open = hub_gate::is_open();
    hub_gate::set_open(false);

    assert_eq!(outcome, Err(WithdrawalError::AccountSignedIn));
    assert!(still_open, "a refused withdrawal leaves the gate alone");
    assert!(requests_seen(&hub).await.is_empty());
    assert_eq!(registration_rows(state.db()).await, 1);
    assert_eq!(relay_rows(state.db()).await, 1);
}
