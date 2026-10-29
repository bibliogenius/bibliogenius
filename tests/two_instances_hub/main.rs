//! Two backends against the disposable dev hub, with assertions on the
//! captured log lines, the local databases and the hub database. Today:
//! the smoke scenario (mailboxes, profiles, a clear push). To come, one
//! lot at a time: pairing and the ended-pairing mark, rotation and wraps
//! with a real 304, a reader whose keys changed.
//!
//! Not part of `cargo test`: the target needs the `it-loopback` feature and
//! a hub started with `make it-up` in bibliogenius-hub.
//!
//!   cargo test --features it-loopback --test two_instances_hub
//!
//! Every test is serial: the hub is shared, and so is `HUB_URL`.

mod harness;

use std::time::Duration;

use harness::{Hub, Instance, entry, logs};
use rust_lib_app::services::hub_directory_service::{HubDirectoryService, PushCatalogOutcome};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn two_libraries_get_a_mailbox_and_a_profile_then_one_pushes_in_clear() {
    let hub = Hub::from_env();
    hub.require_ready().await;
    hub.reset().await;
    let logs = logs();
    logs.clear();

    let owner = Instance::start("owner").await;
    let reader = Instance::start("reader").await;
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
