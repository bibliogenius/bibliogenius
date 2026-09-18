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
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, PaginatorTrait, Set};

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
