//! Integration tests for the relay mailbox session provenance diagnostic.
//!
//! The `relay_session` flag is used by `hub_directory_service::register_or_update`
//! to distinguish a `relay_mailbox_id` minted in the current process from one
//! restored from `my_relay_config` on startup. Only mailbox creation paths
//! may flip the flag to `true`; restoration alone must leave it `false`.

use rust_lib_app::db;
use rust_lib_app::services::relay_poller;
use rust_lib_app::services::relay_session::{self, MailboxProvenance};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// The relay row the poller found when the hub answered 404.
async fn seed_relay_row(db: &DatabaseConnection, relay_url: &str, mailbox_uuid: &str) {
    use sea_orm::{ConnectionTrait, Statement};
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO my_relay_config (id, relay_url, mailbox_uuid, read_token, write_token, created_at)
             VALUES (1, '{relay_url}', '{mailbox_uuid}', 'rtok-old', 'wtok-old', '2026-10-20T00:00:00Z')"
        ),
    ))
    .await
    .expect("seed relay row");
}

async fn mount_mailbox_creation(hub: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/api/relay/mailbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uuid": "mbx-recreated-1",
            "read_token": "rtok-new",
            "write_token": "wtok-new",
        })))
        .mount(hub)
        .await;
}

/// `recreate_mailbox` is the poller's self-healing path when the hub returns
/// 404 on poll. A successful recreate proves that a fresh mailbox now exists,
/// so the session flag must flip to `true` — otherwise the next
/// `register_or_update` would emit a false WARN against a mailbox that was
/// in fact just created.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn recreate_mailbox_marks_session_flag() {
    relay_session::reset_for_tests();
    assert!(
        !relay_session::mailbox_created_this_session(),
        "flag must start unset"
    );

    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    seed_relay_row(&db, &hub.uri(), "mbx-stale").await;

    Mock::given(method("POST"))
        .and(path("/api/relay/mailbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "uuid": "mbx-recreated-1",
            "read_token": "rtok-new",
            "write_token": "wtok-new",
        })))
        .expect(1)
        .mount(&hub)
        .await;

    let new_uuid = relay_poller::recreate_mailbox(&db, &hub.uri(), "mbx-stale")
        .await
        .expect("recreate_mailbox succeeds");

    assert_eq!(new_uuid, "mbx-recreated-1");
    assert!(
        relay_session::mailbox_created_this_session(),
        "recreate_mailbox must mark the session flag so the next \
         register_or_update classifies the mailbox as Fresh",
    );
}

/// When the hub rejects the mailbox creation, the session flag must remain
/// unset — otherwise a subsequent `register_or_update` would misclassify a
/// still-persisted-and-possibly-stale mailbox as fresh.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn recreate_mailbox_failure_leaves_flag_unset() {
    relay_session::reset_for_tests();

    let db = setup_test_db().await;
    let hub = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/relay/mailbox"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&hub)
        .await;

    seed_relay_row(&db, &hub.uri(), "mbx-stale").await;
    let result = relay_poller::recreate_mailbox(&db, &hub.uri(), "mbx-stale").await;
    assert!(result.is_err(), "recreate should fail on 5xx");
    assert!(
        !relay_session::mailbox_created_this_session(),
        "failed recreate must not flip the flag",
    );
}

/// A disconnect while the mailbox was being recreated wins: the relay must
/// not come back on its own.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn recreate_mailbox_does_not_resurrect_a_disconnected_relay() {
    relay_session::reset_for_tests();
    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    mount_mailbox_creation(&hub).await;

    let result = relay_poller::recreate_mailbox(&db, &hub.uri(), "mbx-stale").await;

    assert!(result.is_err(), "no relay row left: nothing to recreate");
    assert!(
        rust_lib_app::api::relay::get_my_relay_config(&db)
            .await
            .is_none(),
        "the disconnected relay stays disconnected",
    );
    assert!(!relay_session::mailbox_created_this_session());
}

/// A setup that replaced the mailbox meanwhile (renewal on the same hub, or
/// another hub) wins over the recreation of the old one.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn recreate_mailbox_keeps_a_mailbox_replaced_meanwhile() {
    relay_session::reset_for_tests();
    let db = setup_test_db().await;
    let hub = MockServer::start().await;
    mount_mailbox_creation(&hub).await;
    seed_relay_row(&db, &hub.uri(), "mbx-from-setup").await;

    let result = relay_poller::recreate_mailbox(&db, &hub.uri(), "mbx-stale").await;

    assert!(result.is_err());
    let row = rust_lib_app::api::relay::get_my_relay_config(&db)
        .await
        .expect("row kept");
    assert_eq!(row.mailbox_uuid, "mbx-from-setup");
}

/// Pure classifier sanity check in the integration-test context (guards
/// against visibility regressions — the helper must remain callable from
/// outside the crate so that downstream binaries / tests can assert on it).
#[test]
#[serial]
fn classify_mailbox_provenance_end_to_end() {
    relay_session::reset_for_tests();

    assert_eq!(
        relay_session::classify_mailbox_provenance(None),
        MailboxProvenance::Absent,
    );
    assert_eq!(
        relay_session::classify_mailbox_provenance(Some("mbx-x")),
        MailboxProvenance::Restored,
    );

    relay_session::mark_mailbox_created_this_session();
    assert_eq!(
        relay_session::classify_mailbox_provenance(Some("mbx-x")),
        MailboxProvenance::Fresh,
    );
}
