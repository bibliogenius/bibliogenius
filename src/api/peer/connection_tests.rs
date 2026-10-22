//! Regression tests for `receive_connection_request`: which stored peer a LAN
//! connection request lands on, and what a re-pairing is allowed to refresh.
//!
//! Background: two libraries can take turns on one `host:port` (a development
//! build and an installed build of the desktop app). Matching on the URL first
//! renamed one into the other and left it with the wrong keys, and a peer
//! already key-exchanged never had its address or relay credentials refreshed,
//! so a recreated mailbox was never learned and its messages died silently.

use super::*;
use crate::db;
use crate::models::peer;
use axum::http::StatusCode;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};

const KEY_A_ED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY_A_X: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const KEY_B_ED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const KEY_B_X: &str = "b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1";
const UUID_A: &str = "94df3033-4a30-4307-8b82-1c5941474b99";
const UUID_B: &str = "a5a1150e-d93d-4e0c-a488-6d867d3f8e39";
const SHARED_URL: &str = "http://192.168.1.25:8000";

async fn setup_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:").await.expect("init db")
}

struct StoredPeer {
    name: &'static str,
    url: &'static str,
    library_uuid: Option<&'static str>,
    keys: Option<(&'static str, &'static str)>,
    mailbox: Option<(&'static str, &'static str)>,
    stale_since: Option<&'static str>,
}

async fn insert_peer(db: &DatabaseConnection, p: StoredPeer) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    peer::ActiveModel {
        name: Set(p.name.to_string()),
        url: Set(p.url.to_string()),
        library_uuid: Set(p.library_uuid.map(str::to_string)),
        public_key: Set(p.keys.map(|(ed, _)| ed.to_string())),
        x25519_public_key: Set(p.keys.map(|(_, x)| x.to_string())),
        key_exchange_done: Set(p.keys.is_some()),
        relay_url: Set(p
            .mailbox
            .map(|_| "https://hub.bibliogenius.org".to_string())),
        mailbox_id: Set(p.mailbox.map(|(m, _)| m.to_string())),
        relay_write_token: Set(p.mailbox.map(|(_, t)| t.to_string())),
        relay_write_token_invalid_at: Set(p.stale_since.map(str::to_string)),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert peer")
    .id
}

fn request(
    name: &str,
    url: &str,
    library_uuid: &str,
    keys: (&str, &str),
    mailbox: (&str, &str),
) -> IncomingConnectionRequest {
    IncomingConnectionRequest {
        name: name.to_string(),
        url: url.to_string(),
        library_uuid: Some(library_uuid.to_string()),
        ed25519_public_key: Some(keys.0.to_string()),
        x25519_public_key: Some(keys.1.to_string()),
        relay_url: Some("https://hub.bibliogenius.org".to_string()),
        mailbox_id: Some(mailbox.0.to_string()),
        relay_write_token: Some(mailbox.1.to_string()),
    }
}

/// Runs the handler with an address that answers `/api/config` with `served_key`.
async fn receive_with(
    db: &DatabaseConnection,
    payload: IncomingConnectionRequest,
    served_key: Option<&str>,
) -> StatusCode {
    let served = served_key.map(str::to_string);
    register_connection_request(db.clone(), payload, move |_url| {
        let served = served.clone();
        async move { served }
    })
    .await
    .status()
}

/// The honest case: the address serves exactly the key the request claims.
async fn receive(db: &DatabaseConnection, payload: IncomingConnectionRequest) {
    let key = payload.ed25519_public_key.clone();
    let status = receive_with(db, payload, key.as_deref()).await;
    assert_eq!(status, StatusCode::OK);
}

async fn all_peers(db: &DatabaseConnection) -> Vec<peer::Model> {
    peer::Entity::find().all(db).await.expect("list peers")
}

#[tokio::test]
async fn another_library_on_the_same_address_becomes_a_second_peer() {
    let db = setup_db().await;
    let dev_id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de Mac Book Prof",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-dev", "token-dev")),
            stale_since: None,
        },
    )
    .await;

    receive(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-store", "token-store"),
        ),
    )
    .await;

    let peers = all_peers(&db).await;
    assert_eq!(
        peers.len(),
        2,
        "the store build is a new peer, not a rename"
    );
    let dev = peers.iter().find(|p| p.id == dev_id).expect("dev row kept");
    assert_eq!(dev.name, "Bibliotheque de Mac Book Prof");
    assert_eq!(dev.library_uuid.as_deref(), Some(UUID_A));
    assert_eq!(dev.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(
        dev.mailbox_id.as_deref(),
        Some("mailbox-dev"),
        "still reachable via relay"
    );
    assert_eq!(
        dev.url,
        format!("relay://{UUID_A}"),
        "the address changed hands; peers.url is UNIQUE"
    );
    let store = peers.iter().find(|p| p.id != dev_id).expect("store row");
    assert_eq!(store.url, SHARED_URL);
    assert_eq!(store.library_uuid.as_deref(), Some(UUID_B));
    assert_eq!(store.public_key.as_deref(), Some(KEY_B_ED));
    assert_eq!(store.x25519_public_key.as_deref(), Some(KEY_B_X));
    assert_eq!(store.mailbox_id.as_deref(), Some("mailbox-store"));
    assert!(store.key_exchange_done);
}

#[tokio::test]
async fn re_pairing_with_the_same_keys_refreshes_address_and_relay_credentials() {
    let db = setup_db().await;
    let id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de demo",
            url: "http://192.168.1.55:8000",
            library_uuid: Some(UUID_B),
            keys: Some((KEY_B_ED, KEY_B_X)),
            mailbox: Some(("mailbox-old", "token-old")),
            stale_since: Some("2026-09-18T13:13:00+00:00"),
        },
    )
    .await;

    receive(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-new", "token-new"),
        ),
    )
    .await;

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 1, "same library, same row");
    let p = &peers[0];
    assert_eq!(p.id, id);
    assert_eq!(p.url, SHARED_URL, "address follows the re-pairing");
    assert_eq!(p.mailbox_id.as_deref(), Some("mailbox-new"));
    assert_eq!(p.relay_write_token.as_deref(), Some("token-new"));
    assert!(
        p.relay_write_token_invalid_at.is_none(),
        "ADR-032 stale gate cleared by the fresh invitation"
    );
    assert_eq!(p.public_key.as_deref(), Some(KEY_B_ED), "keys untouched");
    assert_eq!(p.x25519_public_key.as_deref(), Some(KEY_B_X));
    assert!(p.key_exchange_done);
}

#[tokio::test]
async fn re_pairing_with_different_keys_keeps_the_stored_identity() {
    let db = setup_db().await;
    insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de demo",
            url: SHARED_URL,
            library_uuid: Some(UUID_B),
            keys: Some((KEY_B_ED, KEY_B_X)),
            mailbox: Some(("mailbox-old", "token-old")),
            stale_since: None,
        },
    )
    .await;

    receive(
        &db,
        request(
            "Bibliotheque de demo",
            "http://10.0.0.9:8000",
            UUID_B,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-attacker", "token-attacker"),
        ),
    )
    .await;

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 1);
    let p = &peers[0];
    assert_eq!(
        p.public_key.as_deref(),
        Some(KEY_B_ED),
        "no key rotation over LAN"
    );
    assert_eq!(p.x25519_public_key.as_deref(), Some(KEY_B_X));
    assert_eq!(p.url, SHARED_URL, "address not hijacked");
    assert_eq!(p.mailbox_id.as_deref(), Some("mailbox-old"));
    assert_eq!(p.relay_write_token.as_deref(), Some("token-old"));
}

#[tokio::test]
async fn a_row_without_identity_is_still_adopted_on_a_url_match() {
    let db = setup_db().await;
    let id = insert_peer(
        &db,
        StoredPeer {
            name: "Discovered on the LAN",
            url: SHARED_URL,
            library_uuid: None,
            keys: None,
            mailbox: None,
            stale_since: None,
        },
    )
    .await;

    receive(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-store", "token-store"),
        ),
    )
    .await;

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 1, "legacy plaintext row upgraded in place");
    let p = &peers[0];
    assert_eq!(p.id, id);
    assert_eq!(p.name, "Bibliotheque de demo");
    assert_eq!(p.library_uuid.as_deref(), Some(UUID_B));
    assert_eq!(p.public_key.as_deref(), Some(KEY_B_ED));
    assert!(p.key_exchange_done);
    assert_eq!(p.mailbox_id.as_deref(), Some("mailbox-store"));
}

#[tokio::test]
async fn identity_wins_over_a_stale_address() {
    let db = setup_db().await;
    let dev_id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de Mac Book Prof",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-dev", "token-dev")),
            stale_since: None,
        },
    )
    .await;
    let store_id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de demo",
            url: "http://192.168.1.55:8000",
            library_uuid: Some(UUID_B),
            keys: Some((KEY_B_ED, KEY_B_X)),
            mailbox: Some(("mailbox-old", "token-old")),
            stale_since: None,
        },
    )
    .await;

    receive(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-new", "token-new"),
        ),
    )
    .await;

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 2);
    let dev = peers.iter().find(|p| p.id == dev_id).expect("dev row");
    assert_eq!(dev.name, "Bibliotheque de Mac Book Prof", "identity kept");
    assert_eq!(dev.mailbox_id.as_deref(), Some("mailbox-dev"));
    assert_eq!(dev.url, format!("relay://{UUID_A}"), "address released");
    let store = peers.iter().find(|p| p.id == store_id).expect("store row");
    assert_eq!(store.url, SHARED_URL);
    assert_eq!(store.mailbox_id.as_deref(), Some("mailbox-new"));
    assert_eq!(peer::Entity::find().count(&db).await.expect("count"), 2);
}

#[tokio::test]
async fn refresh_is_refused_when_the_address_does_not_serve_the_claimed_key() {
    let db = setup_db().await;
    insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de demo",
            url: "http://192.168.1.55:8000",
            library_uuid: Some(UUID_B),
            keys: Some((KEY_B_ED, KEY_B_X)),
            mailbox: Some(("mailbox-old", "token-old")),
            stale_since: Some("2026-09-18T13:13:00+00:00"),
        },
    )
    .await;

    // Right keys in the payload (they are public), but the address answers
    // as someone else: nothing about the trusted row may move.
    let status = receive_with(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-attacker", "token-attacker"),
        ),
        Some(KEY_A_ED),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 1);
    let p = &peers[0];
    assert_eq!(p.url, "http://192.168.1.55:8000");
    assert_eq!(p.mailbox_id.as_deref(), Some("mailbox-old"));
    assert_eq!(p.relay_write_token.as_deref(), Some("token-old"));
    assert!(p.relay_write_token_invalid_at.is_some(), "gate untouched");
}

#[tokio::test]
async fn address_hand_over_is_refused_without_proof() {
    let db = setup_db().await;
    let dev_id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de Mac Book Prof",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-dev", "token-dev")),
            stale_since: None,
        },
    )
    .await;

    let status = receive_with(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-store", "token-store"),
        ),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "same answer as update_peer_url"
    );

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 1, "no newcomer");
    assert_eq!(peers[0].id, dev_id);
    assert_eq!(peers[0].url, SHARED_URL, "holder keeps its address");
}

#[tokio::test]
async fn a_blocked_address_is_rejected_before_anything_is_stored() {
    let db = setup_db().await;
    let status = receive_with(
        &db,
        request(
            "Bibliotheque de demo",
            "http://127.0.0.1:8000",
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-store", "token-store"),
        ),
        Some(KEY_B_ED),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(all_peers(&db).await.is_empty());
}

#[tokio::test]
async fn first_key_exchange_keeps_the_stored_address_when_the_new_one_is_refused() {
    let db = setup_db().await;
    let holder_id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de Mac Book Prof",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-dev", "token-dev")),
            stale_since: None,
        },
    )
    .await;
    // A plaintext-era row for the store library, never key-exchanged, found
    // by its library_uuid. It now claims the holder's address without proof.
    let legacy_id = insert_peer(
        &db,
        StoredPeer {
            name: "Bibliotheque de demo",
            url: "http://192.168.1.55:8000",
            library_uuid: Some(UUID_B),
            keys: None,
            mailbox: None,
            stale_since: None,
        },
    )
    .await;

    let status = receive_with(
        &db,
        request(
            "Bibliotheque de demo",
            SHARED_URL,
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-store", "token-store"),
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let peers = all_peers(&db).await;
    assert_eq!(peers.len(), 2);
    let holder = peers.iter().find(|p| p.id == holder_id).expect("holder");
    assert_eq!(holder.url, SHARED_URL, "holder keeps the address");
    let legacy = peers
        .iter()
        .find(|p| p.id == legacy_id)
        .expect("legacy row");
    assert_eq!(
        legacy.url, "http://192.168.1.55:8000",
        "stored address kept"
    );
    assert_eq!(legacy.public_key.as_deref(), Some(KEY_B_ED), "keys adopted");
    assert!(legacy.key_exchange_done);
    assert_eq!(legacy.mailbox_id.as_deref(), Some("mailbox-store"));
}

// ---------------------------------------------------------------------------
// Outbound pairing (`connect`, QR code or invitation link scanned by the
// local user): the same identity rule as the inbound path. A trusted row is
// only rewritten by a pairing that declares its identity; a pairing for
// another library at that row's address gets a row of its own.
// ---------------------------------------------------------------------------

fn outbound(
    name: &str,
    url: &str,
    library_uuid: Option<&str>,
    keys: (&str, &str),
    mailbox: (&str, &str),
) -> ConnectRequest {
    ConnectRequest {
        name: name.to_string(),
        url: url.to_string(),
        public_key: None,
        library_uuid: library_uuid.map(str::to_string),
        ed25519_public_key: Some(keys.0.to_string()),
        x25519_public_key: Some(keys.1.to_string()),
        relay_url: Some("https://hub.bibliogenius.org".to_string()),
        mailbox_id: Some(mailbox.0.to_string()),
        relay_write_token: Some(mailbox.1.to_string()),
    }
}

/// Pairs with the address unreachable (no WiFi): everything comes from the
/// scanned payload, as on a phone pairing over the relay.
async fn pair_outbound(db: &DatabaseConnection, payload: ConnectRequest) -> i32 {
    register_outbound_peer(db, payload, RemoteConfigData::default())
        .await
        .expect("outbound pairing")
        .0
}

async fn peer_by_id(db: &DatabaseConnection, id: i32) -> peer::Model {
    peer::Entity::find_by_id(id)
        .one(db)
        .await
        .expect("find peer")
        .expect("peer exists")
}

#[tokio::test]
async fn scanning_another_library_at_a_trusted_peers_address_keeps_that_peer_intact() {
    // 2026-10-21, A4: the store app and the test app share one phone. A QR
    // for the test app (B) scanned while the store app (A) holds the address
    // must not turn A's row into B.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    // The address is silent at scan time: nothing proves it is B's now, so
    // the trusted holder keeps it and B is stored reachable through its relay.
    let b = pair_outbound(
        &db,
        outbound(
            "Test app",
            SHARED_URL,
            Some(UUID_B),
            (KEY_B_ED, KEY_B_X),
            ("mailbox-b", "token-b"),
        ),
    )
    .await;

    assert_ne!(a, b, "another identity gets its own row");
    let stored_a = peer_by_id(&db, a).await;
    assert_eq!(stored_a.name, "Store app");
    assert_eq!(stored_a.library_uuid.as_deref(), Some(UUID_A));
    assert_eq!(stored_a.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(stored_a.x25519_public_key.as_deref(), Some(KEY_A_X));
    assert_eq!(stored_a.mailbox_id.as_deref(), Some("mailbox-a"));
    assert_eq!(stored_a.relay_write_token.as_deref(), Some("token-a"));
    assert_eq!(
        stored_a.url, SHARED_URL,
        "a silent address stays with its holder"
    );
    let stored_b = peer_by_id(&db, b).await;
    assert_eq!(stored_b.url, format!("relay://{UUID_B}"));
    assert_eq!(stored_b.library_uuid.as_deref(), Some(UUID_B));
    assert_eq!(stored_b.public_key.as_deref(), Some(KEY_B_ED));
    assert_eq!(stored_b.mailbox_id.as_deref(), Some("mailbox-b"));
}

#[tokio::test]
async fn an_address_answering_as_the_scanned_library_is_released_by_its_holder() {
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    let (b, sync_url) = register_outbound_peer(
        &db,
        outbound(
            "Test app",
            SHARED_URL,
            Some(UUID_B),
            (KEY_B_ED, KEY_B_X),
            ("mailbox-b", "token-b"),
        ),
        remote(Some(UUID_B), Some(KEY_B_ED)),
    )
    .await
    .expect("outbound pairing");

    assert_ne!(a, b);
    let stored_a = peer_by_id(&db, a).await;
    assert_eq!(
        stored_a.url,
        format!("relay://{UUID_A}"),
        "the address changed hands; peers.url is UNIQUE"
    );
    assert_eq!(stored_a.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(stored_a.mailbox_id.as_deref(), Some("mailbox-a"));
    let stored_b = peer_by_id(&db, b).await;
    assert_eq!(stored_b.url, SHARED_URL);
    assert_eq!(sync_url.as_deref(), Some(SHARED_URL));
}

#[tokio::test]
async fn scanning_a_payload_without_uuid_keeps_a_trusted_row_with_other_keys() {
    // Older payloads carry keys but no library uuid: the key is identity
    // enough to refuse a URL match on a row holding other keys.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    let b = pair_outbound(
        &db,
        outbound(
            "Test app",
            SHARED_URL,
            None,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-b", "token-b"),
        ),
    )
    .await;

    assert_ne!(a, b);
    let stored_a = peer_by_id(&db, a).await;
    assert_eq!(stored_a.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(stored_a.library_uuid.as_deref(), Some(UUID_A));
    assert_eq!(stored_a.mailbox_id.as_deref(), Some("mailbox-a"));
    assert_eq!(
        stored_a.url, SHARED_URL,
        "a silent address stays with its holder"
    );
    assert_eq!(peer_by_id(&db, b).await.url, format!("relay://{KEY_B_ED}"));
}

#[tokio::test]
async fn a_scan_without_identity_keeps_the_identity_of_the_row_it_matches() {
    // Legacy payload (name and url only) for an address held by a peer that
    // already exchanged keys: nothing may be erased by a payload that
    // declares nothing.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    let paired = pair_outbound(
        &db,
        ConnectRequest {
            name: "Store app".to_string(),
            url: SHARED_URL.to_string(),
            public_key: None,
            library_uuid: None,
            ed25519_public_key: None,
            x25519_public_key: None,
            relay_url: None,
            mailbox_id: None,
            relay_write_token: None,
        },
    )
    .await;

    assert_eq!(paired, a);
    let stored = peer_by_id(&db, a).await;
    assert_eq!(stored.library_uuid.as_deref(), Some(UUID_A));
    assert_eq!(stored.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(stored.x25519_public_key.as_deref(), Some(KEY_A_X));
    assert!(stored.key_exchange_done);
    assert_eq!(stored.mailbox_id.as_deref(), Some("mailbox-a"));
}

#[tokio::test]
async fn scanning_the_same_library_again_refreshes_its_address_and_relay() {
    // The legitimate case: the peer moved port or recreated its mailbox and
    // shows a fresh QR. Matched by uuid, refreshed in place.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-old", "token-old")),
            stale_since: Some("2026-10-21T05:00:00Z"),
        },
    )
    .await;

    let again = pair_outbound(
        &db,
        outbound(
            "Store app",
            "http://192.168.1.25:8010",
            Some(UUID_A),
            (KEY_A_ED, KEY_A_X),
            ("mailbox-new", "token-new"),
        ),
    )
    .await;

    assert_eq!(again, a, "same identity, same row");
    assert_eq!(all_peers(&db).await.len(), 1);
    let stored = peer_by_id(&db, a).await;
    assert_eq!(stored.url, "http://192.168.1.25:8010");
    assert_eq!(stored.mailbox_id.as_deref(), Some("mailbox-new"));
    assert_eq!(stored.relay_write_token.as_deref(), Some("token-new"));
    assert!(stored.relay_write_token_invalid_at.is_none());
}

#[tokio::test]
async fn scanning_adopts_a_row_without_identity_on_a_url_match() {
    // A row from before the key exchange: nothing contradicts, it is adopted.
    let db = setup_db().await;
    let legacy = insert_peer(
        &db,
        StoredPeer {
            name: "Old row",
            url: SHARED_URL,
            library_uuid: None,
            keys: None,
            mailbox: None,
            stale_since: None,
        },
    )
    .await;

    let paired = pair_outbound(
        &db,
        outbound(
            "Store app",
            SHARED_URL,
            Some(UUID_A),
            (KEY_A_ED, KEY_A_X),
            ("mailbox-a", "token-a"),
        ),
    )
    .await;

    assert_eq!(paired, legacy);
    assert_eq!(all_peers(&db).await.len(), 1);
    let stored = peer_by_id(&db, legacy).await;
    assert_eq!(stored.library_uuid.as_deref(), Some(UUID_A));
    assert_eq!(stored.public_key.as_deref(), Some(KEY_A_ED));
    assert!(stored.key_exchange_done);
}

// ---------------------------------------------------------------------------
// Inbound: a request that carries a trusted peer's PUBLIC keys (the hub
// publishes them) with another library uuid, from an address that serves
// nothing. Keys matched the stored row; nothing else may be rewritten
// without proof that the address hosts that identity.
// ---------------------------------------------------------------------------

async fn insert_cached_book(db: &DatabaseConnection, peer_id: i32) {
    use crate::models::peer_book;
    let now = chrono::Utc::now().to_rfc3339();
    peer_book::ActiveModel {
        peer_id: Set(peer_id),
        remote_book_id: Set("book-1".to_string()),
        title: Set("Cached title".to_string()),
        synced_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert cached book");
}

async fn cached_books(db: &DatabaseConnection, peer_id: i32) -> u64 {
    use crate::models::peer_book;
    peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .count(db)
        .await
        .expect("count cached books")
}

#[tokio::test]
async fn public_keys_with_another_uuid_and_no_proof_rewrite_nothing() {
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;
    insert_cached_book(&db, a).await;

    let status = receive_with(
        &db,
        request(
            "Attacker",
            "http://192.168.1.99:8000",
            UUID_B,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-x", "token-x"),
        ),
        None,
    )
    .await;

    assert_ne!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let stored = peer_by_id(&db, a).await;
    assert_eq!(stored.name, "Store app", "name is identity-bound");
    assert_eq!(
        stored.library_uuid.as_deref(),
        Some(UUID_A),
        "uuid is identity-bound"
    );
    assert_eq!(stored.url, "http://192.168.1.25:8000");
    assert_eq!(stored.mailbox_id.as_deref(), Some("mailbox-a"));
    assert_eq!(cached_books(&db, a).await, 1, "cache is identity-bound");
    assert_eq!(
        all_peers(&db).await.len(),
        1,
        "no row is created for a forged identity"
    );
}

#[tokio::test]
async fn a_known_uuid_never_changes_on_the_unauthenticated_path_even_with_proof() {
    // The address serving a public key proves nothing about who holds it:
    // anyone on the LAN can serve A's published keys on its own /api/config.
    // Address and relay may follow that answer (denial of service at worst),
    // the uuid may not: ADR-053 approves hub follows by uuid. A reset peer
    // gets its new uuid through the signed manifest (ADR-030).
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;
    insert_cached_book(&db, a).await;

    receive(
        &db,
        request(
            "Store app, reset",
            "http://192.168.1.25:8000",
            UUID_B,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-new", "token-new"),
        ),
    )
    .await;

    let stored = peer_by_id(&db, a).await;
    assert_eq!(
        stored.library_uuid.as_deref(),
        Some(UUID_A),
        "uuid waits for the signed manifest"
    );
    assert_eq!(
        stored.mailbox_id.as_deref(),
        Some("mailbox-new"),
        "relay follows the proven address"
    );
    assert_eq!(stored.name, "Store app, reset");
    assert_eq!(
        cached_books(&db, a).await,
        1,
        "the cache goes with the uuid"
    );
    assert_eq!(all_peers(&db).await.len(), 1);
}

#[tokio::test]
async fn a_missing_uuid_is_filled_on_the_unauthenticated_path_with_proof() {
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: None,
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    receive(
        &db,
        request(
            "Store app",
            "http://192.168.1.25:8000",
            UUID_A,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-a", "token-a"),
        ),
    )
    .await;

    assert_eq!(
        peer_by_id(&db, a).await.library_uuid.as_deref(),
        Some(UUID_A)
    );
}

// ---------------------------------------------------------------------------
// Outbound: the address named by the QR answers as another library. Its
// answer must not be merged with the scanned identity, and no catalog is
// pulled from it.
// ---------------------------------------------------------------------------

fn remote(uuid: Option<&str>, key: Option<&str>) -> RemoteConfigData {
    RemoteConfigData {
        remote_name: Some("Whoever answered".to_string()),
        library_uuid: uuid.map(str::to_string),
        ed25519_public_key: key.map(str::to_string),
        x25519_public_key: key.map(|_| "c1".repeat(32)),
        relay_url: Some("https://hub.bibliogenius.org".to_string()),
        mailbox_id: Some("mailbox-answered".to_string()),
        relay_write_token: Some("token-answered".to_string()),
        avatar_config: Some("{\"answered\":true}".to_string()),
        ..RemoteConfigData::default()
    }
}

#[tokio::test]
async fn an_address_answering_as_another_library_is_not_merged_into_the_scan() {
    // QR without uuid for A, the address answers as C (a trusted peer):
    // C's row must keep its keys, A gets a row of its own, no sync from C.
    let db = setup_db().await;
    let c = insert_peer(
        &db,
        StoredPeer {
            name: "Peer C",
            url: "http://192.168.1.30:8000",
            library_uuid: Some(UUID_B),
            keys: Some((KEY_B_ED, KEY_B_X)),
            mailbox: Some(("mailbox-c", "token-c")),
            stale_since: None,
        },
    )
    .await;

    let (a, sync_url) = register_outbound_peer(
        &db,
        outbound(
            "Peer A",
            "http://192.168.1.30:8000",
            None,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-a", "token-a"),
        ),
        remote(Some(UUID_B), Some(KEY_B_ED)),
    )
    .await
    .expect("outbound pairing");

    assert_ne!(a, c);
    let stored_c = peer_by_id(&db, c).await;
    assert_eq!(stored_c.public_key.as_deref(), Some(KEY_B_ED));
    assert_eq!(stored_c.name, "Peer C");
    assert_eq!(stored_c.mailbox_id.as_deref(), Some("mailbox-c"));
    let stored_a = peer_by_id(&db, a).await;
    assert_eq!(stored_a.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(stored_a.library_uuid, None, "the answer's uuid is not A's");
    assert_eq!(stored_a.mailbox_id.as_deref(), Some("mailbox-a"));
    assert_eq!(
        stored_a.avatar_config, None,
        "the answer's avatar is not A's"
    );
    assert!(
        sync_url.is_none(),
        "no catalog is pulled from an address that is not A"
    );
}

#[tokio::test]
async fn a_reassigned_address_does_not_move_the_scanned_peer_onto_it() {
    // QR of A (uuid and keys) while its old address now answers as B.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-old", "token-old")),
            stale_since: None,
        },
    )
    .await;

    let (again, sync_url) = register_outbound_peer(
        &db,
        outbound(
            "Store app",
            "http://192.168.1.40:8000",
            Some(UUID_A),
            (KEY_A_ED, KEY_A_X),
            ("mailbox-new", "token-new"),
        ),
        remote(Some(UUID_B), Some(KEY_B_ED)),
    )
    .await
    .expect("outbound pairing");

    assert_eq!(again, a);
    let stored = peer_by_id(&db, a).await;
    assert_eq!(
        stored.url, "http://192.168.1.25:8000",
        "the address is not A's"
    );
    assert_eq!(
        stored.mailbox_id.as_deref(),
        Some("mailbox-new"),
        "the QR's relay is A's"
    );
    assert_eq!(stored.avatar_config, None);
    assert!(sync_url.is_none());
}

#[tokio::test]
async fn an_address_answering_as_the_scanned_library_is_merged_as_before() {
    let db = setup_db().await;
    let (a, sync_url) = register_outbound_peer(
        &db,
        outbound(
            "",
            "http://192.168.1.25:8000",
            None,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-a", "token-a"),
        ),
        remote(Some(UUID_A), Some(KEY_A_ED)),
    )
    .await
    .expect("outbound pairing");
    let stored = peer_by_id(&db, a).await;
    assert_eq!(stored.name, "Whoever answered");
    assert_eq!(stored.library_uuid.as_deref(), Some(UUID_A));
    assert!(stored.avatar_config.is_some());
    assert_eq!(sync_url.as_deref(), Some("http://192.168.1.25:8000"));
}

#[tokio::test]
async fn a_missing_uuid_is_not_filled_without_proof() {
    // Keyed row without uuid (older pairing): a request citing its public
    // keys from a silent address must not hand it a uuid (ADR-053 follows).
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: None,
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    receive_with(
        &db,
        request(
            "Attacker",
            "http://192.168.1.99:8000",
            UUID_B,
            (KEY_A_ED, KEY_A_X),
            ("mailbox-x", "token-x"),
        ),
        None,
    )
    .await;

    assert_eq!(peer_by_id(&db, a).await.library_uuid, None);
}

#[tokio::test]
async fn a_legacy_scan_never_takes_keys_from_the_address_answer() {
    // QR with name and url only; the address answers with A's uuid (public)
    // and other keys. Keys come from the scan or from nothing.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: SHARED_URL,
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    let (paired, _) = register_outbound_peer(
        &db,
        ConnectRequest {
            name: "Store app".to_string(),
            url: SHARED_URL.to_string(),
            public_key: None,
            library_uuid: None,
            ed25519_public_key: None,
            x25519_public_key: None,
            relay_url: None,
            mailbox_id: None,
            relay_write_token: None,
        },
        remote(Some(UUID_A), Some(KEY_B_ED)),
    )
    .await
    .expect("outbound pairing");

    assert_eq!(paired, a);
    let stored = peer_by_id(&db, a).await;
    assert_eq!(stored.public_key.as_deref(), Some(KEY_A_ED));
    assert_eq!(stored.x25519_public_key.as_deref(), Some(KEY_A_X));
    assert_eq!(stored.mailbox_id.as_deref(), Some("mailbox-a"));
}

#[tokio::test]
async fn an_answer_contradicting_the_matched_row_moves_nothing_and_syncs_nothing() {
    // Legacy scan (name and url) of a new address; the address answers with
    // A's uuid but other keys. A is matched by that uuid: it must keep its
    // address, and no catalog is pulled from an address that is not A.
    let db = setup_db().await;
    let a = insert_peer(
        &db,
        StoredPeer {
            name: "Store app",
            url: "http://192.168.1.25:8000",
            library_uuid: Some(UUID_A),
            keys: Some((KEY_A_ED, KEY_A_X)),
            mailbox: Some(("mailbox-a", "token-a")),
            stale_since: None,
        },
    )
    .await;

    let (paired, sync_url) = register_outbound_peer(
        &db,
        ConnectRequest {
            name: "Store app".to_string(),
            url: "http://192.168.1.40:8000".to_string(),
            public_key: None,
            library_uuid: None,
            ed25519_public_key: None,
            x25519_public_key: None,
            relay_url: None,
            mailbox_id: None,
            relay_write_token: None,
        },
        remote(Some(UUID_A), Some(KEY_B_ED)),
    )
    .await
    .expect("outbound pairing");

    assert_eq!(paired, a);
    assert_eq!(peer_by_id(&db, a).await.url, "http://192.168.1.25:8000");
    assert!(sync_url.is_none());
}

#[tokio::test]
async fn a_lan_request_from_a_library_whose_pairing_we_ended_waits_for_the_owner() {
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_B), None)
        .await
        .unwrap();

    receive(
        &db,
        request(
            "Ex peer",
            "http://192.168.1.40:8000",
            UUID_B,
            (KEY_B_ED, KEY_B_X),
            ("mailbox-b", "token-b"),
        ),
    )
    .await;

    let stored = peer::Entity::find()
        .filter(peer::Column::LibraryUuid.eq(UUID_B))
        .one(&db)
        .await
        .unwrap()
        .expect("created");
    assert_eq!(stored.connection_status, "pending");
    assert!(!stored.auto_approve);
}

#[tokio::test]
async fn a_lan_request_from_an_ex_peer_leaving_out_its_uuid_waits_for_the_owner() {
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_B), Some(KEY_B_ED))
        .await
        .unwrap();
    let mut payload = request(
        "Ex peer",
        "http://192.168.1.40:8000",
        UUID_B,
        (KEY_B_ED, KEY_B_X),
        ("mailbox-b", "token-b"),
    );
    payload.library_uuid = None;

    receive(&db, payload).await;

    let stored = peer::Entity::find()
        .filter(peer::Column::PublicKey.eq(KEY_B_ED))
        .one(&db)
        .await
        .unwrap()
        .expect("created");
    assert_eq!(stored.connection_status, "pending");
    assert!(!stored.auto_approve);
}

async fn pending_peer(db: &DatabaseConnection, uuid: &str, ed25519: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    peer::ActiveModel {
        name: Set(format!("peer {uuid}")),
        url: Set(format!("relay://{uuid}")),
        library_uuid: Set(Some(uuid.to_string())),
        public_key: Set(Some(ed25519.to_string())),
        connection_status: Set("pending".to_string()),
        auto_approve: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("insert peer")
    .id
}

#[tokio::test]
async fn accepting_an_ex_peer_restores_its_pairing() {
    use axum::extract::{Json, Path, State};
    use axum::response::IntoResponse;
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_B), Some(KEY_B_ED))
        .await
        .unwrap();
    let id = pending_peer(&db, UUID_B, KEY_B_ED).await;

    let status = update_peer_status(
        State(db.clone()),
        Path(id),
        Json(serde_json::from_value(serde_json::json!({ "status": "active" })).unwrap()),
    )
    .await
    .into_response()
    .status();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(peer_by_id(&db, id).await.connection_status, "accepted");
    assert!(
        !crate::services::hub_catalog_readers::pairing_ended(&db, Some(UUID_B), None)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn turning_validation_off_never_lets_an_ex_peer_back_in() {
    use axum::extract::State;
    use axum::response::IntoResponse;
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_B), Some(KEY_B_ED))
        .await
        .unwrap();
    let ex = pending_peer(&db, UUID_B, KEY_B_ED).await;
    let newcomer = pending_peer(&db, UUID_A, KEY_A_ED).await;

    let status = auto_approve_all_peers(State(db.clone()))
        .await
        .into_response()
        .status();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        peer_by_id(&db, newcomer).await.connection_status,
        "accepted"
    );
    assert_eq!(peer_by_id(&db, ex).await.connection_status, "pending");
    assert!(
        crate::services::hub_catalog_readers::pairing_ended(&db, Some(UUID_B), None)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_failed_acceptance_keeps_the_ex_peer_held_back() {
    use axum::extract::{Json, Path, State};
    use axum::response::IntoResponse;
    use sea_orm::ConnectionTrait;
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_B), Some(KEY_B_ED))
        .await
        .unwrap();
    let id = pending_peer(&db, UUID_B, KEY_B_ED).await;
    db.execute_unprepared(
        "CREATE TRIGGER refuse_acceptance BEFORE UPDATE ON peers
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .await
    .unwrap();

    let status = update_peer_status(
        State(db.clone()),
        Path(id),
        Json(serde_json::from_value(serde_json::json!({ "status": "active" })).unwrap()),
    )
    .await
    .into_response()
    .status();

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(peer_by_id(&db, id).await.connection_status, "pending");
    assert!(
        crate::services::hub_catalog_readers::pairing_ended(&db, Some(UUID_B), None)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn adding_an_ex_peer_by_its_address_restores_its_pairing() {
    // Nothing in the payload but an address: the library that answers
    // there is the one the owner chose to add.
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_B), Some(KEY_B_ED))
        .await
        .unwrap();

    register_outbound_peer(
        &db,
        ConnectRequest {
            name: "Ex peer".to_string(),
            url: "http://192.168.1.40:8000".to_string(),
            public_key: None,
            library_uuid: None,
            ed25519_public_key: None,
            x25519_public_key: None,
            relay_url: None,
            mailbox_id: None,
            relay_write_token: None,
        },
        remote(Some(UUID_B), Some(KEY_B_ED)),
    )
    .await
    .expect("outbound pairing");

    assert!(
        !crate::services::hub_catalog_readers::pairing_ended(&db, Some(UUID_B), Some(KEY_B_ED))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_address_answering_for_another_library_restores_nothing_of_it() {
    // The scan names B by uuid; the address answers with nothing but the
    // signing key of A. Mixing the two would lift A's mark.
    let db = setup_db().await;
    crate::services::hub_catalog_readers::record_pairing_end(&db, Some(UUID_A), Some(KEY_A_ED))
        .await
        .unwrap();

    let _ = register_outbound_peer(
        &db,
        ConnectRequest {
            name: "Newcomer".to_string(),
            url: "http://192.168.1.40:8000".to_string(),
            public_key: None,
            library_uuid: Some(UUID_B.to_string()),
            ed25519_public_key: None,
            x25519_public_key: None,
            relay_url: None,
            mailbox_id: None,
            relay_write_token: None,
        },
        remote(None, Some(KEY_A_ED)),
    )
    .await;

    assert!(
        crate::services::hub_catalog_readers::pairing_ended(&db, Some(UUID_A), Some(KEY_A_ED))
            .await
            .unwrap()
    );
}
