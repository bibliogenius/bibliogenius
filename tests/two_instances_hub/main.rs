//! Two backends against the disposable dev hub, with assertions on the
//! captured log lines, the local databases and the hub database. Today:
//! the smoke scenario (mailboxes, profiles, a clear push), the pairing
//! over the LAN, the ended-pairing mark and the ex-peer held back, the
//! encrypted catalog with its wraps, the wraps sent again after a real
//! 304, the rotation after a reader left and after the owner removed an
//! offline reader.
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

/// A two-entry catalog with one well-known title. Built once per test:
/// every call draws new book ids, and a catalog with other ids is another
/// catalog for the hub.
fn small_catalog() -> Vec<rust_lib_app::services::hub_directory_service::CatalogEntry> {
    vec![
        entry("9782264024848", "Martin Eden"),
        entry("9782070360024", "The Stranger"),
    ]
}

/// Waits until the hub shows `follower`'s follow of `followed` without a
/// wrap: a withdrawal runs in the background of the owner's gesture.
async fn wait_for_wrap_withdrawn(hub: &Hub, follower: &str, followed: &str) -> String {
    let started = std::time::Instant::now();
    loop {
        let (status, wrap) = hub
            .follow_state(follower, followed)
            .await
            .expect("the follow is still on the hub");
        if wrap.is_empty() {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wrap was not withdrawn within 5 s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn encrypted_push_wraps_the_reader_rewraps_after_a_real_304_and_rotates_when_it_leaves() {
    let (hub, logs, owner, reader) = paired_libraries().await;
    let reader_short = &reader.library_uuid[..8];
    let catalog = small_catalog();
    reader.follow(&owner).await;
    owner.approve_follow_from(reader.node_id()).await;

    // First encrypted push: the key is made, the paired follower joins the
    // readers list and receives its wrap, the hub stores a ciphertext.
    let (count, outcome) = owner.push(catalog.clone(), true).await;
    assert_eq!((count, outcome), (2, PushCatalogOutcome::Pushed));
    logs.wait_for(
        &format!("catalog reader {reader_short} let in ("),
        Duration::from_secs(1),
    )
    .await;
    let (format, key_id, payload) = hub
        .catalog_fingerprint(owner.node_id())
        .await
        .expect("a catalog on the hub");
    assert_eq!(format, "e2ee-v1");
    assert_eq!(key_id.len(), 32, "a 128-bit key id in the blob header");
    assert!(
        !hub.catalog_shows(owner.node_id(), "Martin Eden").await,
        "no title in clear on the hub"
    );
    let (status, wrap) = hub
        .follow_state(reader.node_id(), owner.node_id())
        .await
        .expect("the follow");
    assert_eq!(status, "active");
    assert!(!wrap.is_empty(), "the reader holds a wrap");
    let titles: Vec<String> = reader
        .read_catalog(&owner)
        .await
        .expect("the reader opens the catalog")
        .into_iter()
        .map(|e| e.title)
        .collect();
    assert!(titles.contains(&"Martin Eden".to_string()), "{titles:?}");

    // A12: the local push state is lost, the same catalog goes out again,
    // the hub answers 304 and the wraps are handed out once more.
    owner.forget_push_state().await;
    let (_, outcome) = owner.push(catalog.clone(), true).await;
    assert_eq!(outcome, PushCatalogOutcome::SkippedRemote, "a real 304");
    logs.wait_for("outcome=SkippedRemote", Duration::from_secs(1))
        .await;
    assert!(
        logs.line_with("catalog key wraps not delivered").is_none(),
        "the wraps went out"
    );
    let (_, key_id_after_304, payload_after_304) = hub
        .catalog_fingerprint(owner.node_id())
        .await
        .expect("still stored");
    assert_eq!(payload_after_304, payload, "the hub kept its ciphertext");
    assert_eq!(key_id_after_304, key_id, "same key");
    let (_, wrap_after_304) = hub
        .follow_state(reader.node_id(), owner.node_id())
        .await
        .expect("the follow");
    assert_ne!(wrap_after_304, wrap, "a fresh wrap of the same key");
    assert!(
        reader
            .read_catalog(&owner)
            .await
            .expect("the fresh wrap opens the kept ciphertext")
            .iter()
            .any(|e| e.title == "Martin Eden"),
        "the reader still reads after the owner lost its push state"
    );

    // A9: the reader unfollows; the next encrypted push rotates the key
    // first and the ex-reader cannot read the new ciphertext.
    reader.unfollow(&owner).await;
    let (_, outcome) = owner.push(catalog, true).await;
    assert_eq!(
        outcome,
        PushCatalogOutcome::Pushed,
        "a new key, a new digest"
    );
    let revoked = logs
        .wait_for(
            &format!("catalog reader {reader_short} revoked (follower left)"),
            Duration::from_secs(1),
        )
        .await;
    assert!(revoked.contains("key rotation due"), "{revoked}");
    logs.wait_for(
        "catalog key rotated after a reader left",
        Duration::from_secs(1),
    )
    .await;
    let (_, key_id_rotated, payload_rotated) = hub
        .catalog_fingerprint(owner.node_id())
        .await
        .expect("still stored");
    assert_ne!(key_id_rotated, key_id, "another key in the blob header");
    assert_ne!(payload_rotated, payload);
    assert!(
        hub.follow_state(reader.node_id(), owner.node_id())
            .await
            .is_none(),
        "the follow is gone from the hub"
    );
    // Back as a follower before any new push: the hub serves the new
    // ciphertext, and the reader holds no wrap of the key it is under.
    reader.follow(&owner).await;
    owner.approve_follow_from(reader.node_id()).await;
    assert_eq!(
        hub.follow_state(reader.node_id(), owner.node_id())
            .await
            .expect("the follow is back"),
        ("active".to_string(), String::new()),
        "no wrap until the next push"
    );
    assert!(
        reader.read_catalog(&owner).await.is_err(),
        "the ex-reader cannot open the ciphertext under the new key"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn removing_an_offline_reader_withdraws_its_wrap_and_rotates_without_its_help() {
    let (hub, logs, owner, reader) = paired_libraries().await;
    let reader_short = &reader.library_uuid[..8];
    let catalog = small_catalog();
    reader.follow(&owner).await;
    owner.approve_follow_from(reader.node_id()).await;
    let (_, outcome) = owner.push(catalog.clone(), true).await;
    assert_eq!(outcome, PushCatalogOutcome::Pushed);
    let (_, key_id, payload) = hub
        .catalog_fingerprint(owner.node_id())
        .await
        .expect("a catalog on the hub");
    let (_, wrap) = hub
        .follow_state(reader.node_id(), owner.node_id())
        .await
        .expect("the follow");
    assert!(!wrap.is_empty(), "the reader holds a wrap");

    // N1a: the reader is unreachable; the owner removes it. The wrap is
    // withdrawn at once, the departure is counted, and the follow the
    // reader cannot drop itself stays active on the hub.
    reader.go_offline();
    let reader_on_owner = owner.peer_row(reader.node_id()).await.expect("paired");
    owner.delete_peer(reader_on_owner.id).await;
    let ended = logs
        .wait_for(
            &format!("pairing with {reader_short} ended"),
            Duration::from_secs(1),
        )
        .await;
    assert!(ended.contains("catalog key rotation due"), "{ended}");
    let status = wait_for_wrap_withdrawn(&hub, reader.node_id(), owner.node_id()).await;
    assert_eq!(status, "active", "the reader never unfollowed");

    // The next encrypted push rotates and hands no wrap to the ex-reader.
    let (_, outcome) = owner.push(catalog, true).await;
    assert_eq!(outcome, PushCatalogOutcome::Pushed);
    logs.wait_for(
        "catalog key rotated after a reader left",
        Duration::from_secs(1),
    )
    .await;
    let (_, key_id_rotated, payload_rotated) = hub
        .catalog_fingerprint(owner.node_id())
        .await
        .expect("still stored");
    assert_ne!(key_id_rotated, key_id, "another key in the blob header");
    assert_ne!(payload_rotated, payload, "a new ciphertext");
    let (status, wrap_after) = hub
        .follow_state(reader.node_id(), owner.node_id())
        .await
        .expect("the follow");
    assert_eq!(status, "active");
    assert_eq!(wrap_after, "", "no wrap of the new key for the ex-reader");
    assert_eq!(
        logs.text()
            .matches(&format!("catalog reader {reader_short} let in ("))
            .count(),
        1,
        "let in once, at the first push, never again"
    );
}
