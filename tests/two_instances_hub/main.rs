//! Two backends against the disposable dev hub, with assertions on the
//! captured log lines, the local databases and the hub database. Today:
//! the smoke scenario (mailboxes, profiles, a clear push), the pairing
//! over the LAN, the ended-pairing mark and the ex-peer held back. To
//! come, one lot at a time: rotation and wraps with a real 304, a reader
//! whose keys changed.
//!
//! Not part of `cargo test`: the target needs the `it-loopback` feature and
//! a hub started with `make it-up` in bibliogenius-hub.
//!
//!   cargo test --features it-loopback --test two_instances_hub
//!
//! Every test is serial: the hub is shared, and so is `HUB_URL`. Every test
//! runs on the multi-thread runtime: the E2EE nonce store blocks in place.

mod harness;

use std::time::Duration;

use harness::{Hub, Instance, Logs, entry, logs};
use rust_lib_app::services::hub_catalog_readers::pairing_ended;
use rust_lib_app::services::hub_directory_service::{HubDirectoryService, PushCatalogOutcome};
use serial_test::serial;

/// The relay answer wait of the tests: long enough for a deposit, short
/// enough that a message nobody answers does not stall the test.
const RELAY_WAIT: Duration = Duration::from_secs(2);

/// A clean hub and two fresh backends, the log capture emptied.
async fn two_libraries() -> (Hub, &'static Logs, Instance, Instance) {
    let hub = Hub::from_env();
    hub.require_ready().await;
    hub.reset().await;
    let logs = logs();
    logs.clear();
    let owner = Instance::start("owner").await;
    let reader = Instance::start("reader").await;
    (hub, logs, owner, reader)
}

/// Two libraries on the hub, each with a mailbox and a profile, the owner
/// listed with approval required, and paired with each other over the LAN
/// (the owner scanned the reader).
async fn paired_libraries() -> (Hub, &'static Logs, Instance, Instance) {
    let (hub, logs, owner, reader) = two_libraries().await;
    owner.setup_relay(&hub).await;
    reader.setup_relay(&hub).await;
    owner.register("Owner library", true, true).await;
    reader.register("Reader library", false, false).await;
    owner.pair_with(&reader).await;
    (hub, logs, owner, reader)
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn two_libraries_get_a_mailbox_and_a_profile_then_one_pushes_in_clear() {
    let (hub, logs, owner, reader) = two_libraries().await;
    for instance in [&owner, &reader] {
        let status = reqwest::get(format!("{}/api/health", instance.url()))
            .await
            .expect("backend answers")
            .status();
        assert!(status.is_success(), "[{}] health: {status}", instance.name);
    }

    owner.setup_relay(&hub).await;
    reader.setup_relay(&hub).await;
    logs.wait_for(
        "directory config preserved (hub unchanged)",
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(hub.psql("SELECT count(*) FROM relay_mailboxes").await, "2");

    owner.register("Owner library", true, true).await;
    reader.register("Reader library", false, false).await;
    let profiles = hub
        .psql(&format!(
            "SELECT node_id, is_listed, requires_approval FROM library_profiles
              WHERE node_id IN ('{}', '{}') ORDER BY display_name",
            owner.node_id(),
            reader.node_id()
        ))
        .await;
    assert_eq!(
        profiles,
        format!("{}|t|t\n{}|f|f", owner.node_id(), reader.node_id())
    );

    let (count, outcome) = owner
        .push(
            vec![
                entry("9782264024848", "Martin Eden"),
                entry("", "Untitled draft"),
            ],
            false,
        )
        .await;
    assert_eq!((count, outcome), (2, PushCatalogOutcome::Pushed));
    let line = logs
        .wait_for("hub catalog sync outcome", Duration::from_secs(2))
        .await;
    assert!(
        line.contains("outcome=Pushed") && line.contains("encrypted=false"),
        "{line}"
    );
    // C1: what the hub stores for a clear push.
    let stored = hub
        .psql(&format!(
            "SELECT coalesce(catalog_format, ''), (catalog_payload ILIKE '%Martin Eden%')
               FROM cached_catalogs WHERE node_id = '{}'",
            owner.node_id()
        ))
        .await;
    assert_eq!(stored, "|t", "no format, the title readable");
    let cfg = HubDirectoryService::get_config(&owner.db)
        .await
        .unwrap()
        .expect("registered");
    assert!(
        cfg.last_catalog_pushed_at.is_some(),
        "a confirmed push is remembered locally"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn pairing_over_the_lan_exchanges_keys_and_e2ee_messages() {
    let (_hub, logs, owner, reader) = paired_libraries().await;

    let reader_on_owner = owner
        .peer_row(reader.node_id())
        .await
        .expect("the owner stored the reader");
    assert_eq!(reader_on_owner.connection_status, "accepted");
    assert!(
        reader_on_owner.key_exchange_done,
        "keys exchanged on the owner"
    );
    assert_eq!(reader_on_owner.url, reader.url());
    let owner_on_reader = reader
        .peer_row(owner.node_id())
        .await
        .expect("the reader stored the owner");
    assert_eq!(owner_on_reader.connection_status, "accepted");
    assert!(
        owner_on_reader.key_exchange_done,
        "keys exchanged on the reader"
    );
    assert!(
        owner_on_reader.mailbox_id.is_some() && owner_on_reader.relay_write_token.is_some(),
        "the handshake carried the owner's relay credentials"
    );
    let created = logs
        .wait_for("Peer: Created new peer 'owner'", Duration::from_secs(1))
        .await;
    assert!(
        created.contains("e2ee=true") && created.contains("status=accepted"),
        "{created}"
    );

    // A real exchange under the exchanged keys, direct over the LAN.
    let reply = reader
        .send_e2ee(
            &owner_on_reader,
            "book_sync_request",
            serde_json::json!({}),
            RELAY_WAIT,
        )
        .await
        .expect("send")
        .expect("E2EE path taken")
        .expect("answered directly");
    assert_eq!(reply.message_type, "book_sync_response");
    // By row id: the background sync renames the row after the library
    // name the peer announces.
    let received = logs
        .wait_for(
            "E2EE: Received 'book_sync_request' from peer",
            Duration::from_secs(1),
        )
        .await;
    assert!(
        received.ends_with(&format!("({})", reader_on_owner.id)),
        "{received}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_ended_pairing_holds_the_ex_peer_back_until_the_owner_accepts_it_again() {
    let (_hub, logs, owner, reader) = paired_libraries().await;
    let reader_on_owner = owner.peer_row(reader.node_id()).await.expect("paired");
    let reader_short = &reader.library_uuid[..8];

    // The owner removes the reader: the mark is set with the row deleted,
    // the reader learns it over the LAN and drops the owner. The handler
    // notifies the reader before deleting locally; the reader then asks the
    // owner to confirm, and the deletion has taken the pool's single
    // connection long before that round trip lands.
    owner.delete_peer(reader_on_owner.id).await;
    let ended = logs
        .wait_for(
            &format!("pairing with {reader_short} ended"),
            Duration::from_secs(1),
        )
        .await;
    // Due because a departure is counted even for a library that never was
    // on the readers list (no wrap was ever handed out here): the row is
    // the mark alone, revoked with no pinned key.
    assert!(ended.contains("catalog key rotation due"), "{ended}");
    assert_eq!(
        owner.catalog_reader(reader.node_id()).await,
        Some((Some("pairing ended".to_string()), String::new())),
        "the mark is a revoked row without a key"
    );
    assert!(
        pairing_ended(&owner.db, Some(reader.node_id()), None)
            .await
            .unwrap(),
        "the mark is set"
    );
    assert!(
        owner.peer_row(reader.node_id()).await.is_none(),
        "the owner's row of the reader is gone"
    );
    logs.wait_for(
        "removed via disconnect notification",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        reader.peer_row(owner.node_id()).await.is_none(),
        "the reader dropped the owner on the disconnect notice"
    );

    // The ex-peer pairs again on its own: it waits for the owner although
    // connection validation is off, and the mark stays.
    reader.pair_with(&owner).await;
    let created = logs
        .wait_for("Peer: Created new peer 'reader'", Duration::from_secs(1))
        .await;
    assert!(created.contains("status=pending"), "{created}");
    let reader_on_owner = owner.peer_row(reader.node_id()).await.expect("pending row");
    assert_eq!(reader_on_owner.connection_status, "pending");
    assert!(
        pairing_ended(&owner.db, Some(reader.node_id()), None)
            .await
            .unwrap(),
        "the mark survives the new request"
    );

    // Its messages are refused on the LAN (403, no relay fallback)...
    let owner_on_reader = reader.peer_row(owner.node_id()).await.expect("owner row");
    let direct = reader
        .send_e2ee(
            &owner_on_reader,
            "book_sync_request",
            serde_json::json!({}),
            RELAY_WAIT,
        )
        .await;
    assert!(direct.is_err(), "refused: {direct:?}");
    logs.wait_for(
        &format!(
            "E2EE: refused book_sync_request from peer {} whose pairing was ended",
            reader_on_owner.id
        ),
        Duration::from_secs(1),
    )
    .await;
    // ...and dropped when they come through the relay.
    reader.state.mark_peer_direct_failed(owner_on_reader.id);
    let relayed = reader
        .send_e2ee(
            &owner_on_reader,
            "book_sync_request",
            serde_json::json!({}),
            RELAY_WAIT,
        )
        .await
        .expect("deposited in the owner's mailbox");
    assert!(matches!(relayed, Some(None)), "no answer comes back");
    owner.poll_relay().await;
    logs.wait_for(
        &format!(
            "Relay poller: dropped 'book_sync_request' from peer {} whose pairing was ended",
            reader_on_owner.id
        ),
        Duration::from_secs(1),
    )
    .await;

    // The owner accepts the request: the mark is lifted with it, and the
    // next message goes through.
    owner.accept_peer(reader_on_owner.id).await;
    logs.wait_for(
        "ended pairing restored by the owner",
        Duration::from_secs(1),
    )
    .await;
    assert!(
        !pairing_ended(&owner.db, Some(reader.node_id()), None)
            .await
            .unwrap(),
        "the mark is lifted"
    );
    reader.state.clear_peer_direct_failed(owner_on_reader.id);
    let reply = reader
        .send_e2ee(
            &owner_on_reader,
            "book_sync_request",
            serde_json::json!({}),
            RELAY_WAIT,
        )
        .await
        .expect("send")
        .expect("E2EE path taken")
        .expect("answered directly");
    assert_eq!(reply.message_type, "book_sync_response");
}
