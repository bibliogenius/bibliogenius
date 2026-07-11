//! The embedded Axum server listens on 0.0.0.0 because LAN peers must reach the
//! catalogue and the P2P receivers. On that same listener the OWNER surface
//! (contacts, loans, copies, collections, import/export, peer management, ...)
//! must not be served to strangers on the network: those endpoints carry
//! personal data and mutate the library, yet took no credential.
//!
//! The fix splits the router in two on the one listener:
//!   - a PUBLIC allow-list (catalogue reads + peer receivers) stays reachable
//!     from any host, exactly as the P2P design needs;
//!   - every OWNER route sits behind a loopback guard (loopback source AND no
//!     browser `Origin`, reusing the mechanism that already protects the MCP
//!     endpoint), so only the local native client reaches it.
//!
//! These tests pin both halves: the owner surface rejects a non-loopback caller
//! (and a loopback browser page), while a peer still reaches the catalogue and
//! the handshake/receiver routes.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use rust_lib_app::api::api_router;
use rust_lib_app::db;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use std::net::SocketAddr;
use tower::ServiceExt;

const LOOPBACK: &str = "127.0.0.1:54321";
const LAN_PEER: &str = "192.168.1.50:54321";

async fn setup_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory")
}

async fn insert_public_book(db: &DatabaseConnection, title: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_string()),
        isbn: Set(Some("9780000000123".to_string())),
        owned: Set(true),
        private: Set(false),
        reading_status: Set("to_read".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book.insert(db).await.expect("insert book").id
}

/// Build a request against the real, fully-assembled router. `peer` is written
/// into the request extensions the way `into_make_service_with_connect_info`
/// would at runtime; `origin` models a browser page's `Origin` header.
fn request(
    method: &str,
    uri: &str,
    peer: Option<SocketAddr>,
    origin: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    let mut req = builder.body(Body::empty()).expect("request");
    if let Some(peer) = peer {
        req.extensions_mut().insert(ConnectInfo(peer));
    }
    req
}

async fn status_of(
    db: DatabaseConnection,
    method: &str,
    uri: &str,
    peer: Option<SocketAddr>,
    origin: Option<&str>,
) -> StatusCode {
    api_router(db)
        .oneshot(request(method, uri, peer, origin))
        .await
        .expect("response")
        .status()
}

// ── Owner surface: rejected off-loopback (regression: was 200 for anyone) ────

#[tokio::test]
async fn contacts_list_is_refused_from_the_lan() {
    let db = setup_db().await;
    let status = status_of(
        db,
        "GET",
        "/contacts",
        Some(LAN_PEER.parse().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the contact list must not be served to a LAN host"
    );
}

#[tokio::test]
async fn borrowed_copies_are_refused_from_the_lan() {
    let db = setup_db().await;
    let status = status_of(
        db,
        "GET",
        "/copies/borrowed",
        Some(LAN_PEER.parse().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn loans_list_is_refused_from_the_lan() {
    let db = setup_db().await;
    let status = status_of(db, "GET", "/loans", Some(LAN_PEER.parse().unwrap()), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn creating_a_copy_is_refused_from_the_lan() {
    // A write must be blocked before it reaches the handler.
    let db = setup_db().await;
    let status = status_of(db, "POST", "/copies", Some(LAN_PEER.parse().unwrap()), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn listing_peers_is_refused_from_the_lan() {
    // `GET /peers` returns the owner's peer list (personal data); no remote
    // device calls it (the hub-directory lookup targets HUB_URL, not this router).
    let db = setup_db().await;
    let status = status_of(db, "GET", "/peers", Some(LAN_PEER.parse().unwrap()), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_operation_log_pull_is_refused_from_the_lan() {
    // `GET /peers/pull` returns the entire operation_log. It served a shelved
    // device-to-device sync feature and has no live caller, so it must not be
    // reachable from another host.
    let db = setup_db().await;
    let status = status_of(
        db,
        "GET",
        "/peers/pull",
        Some(LAN_PEER.parse().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the operation log must not be dumped to a LAN host"
    );
}

#[tokio::test]
async fn the_owner_surface_is_refused_from_a_loopback_browser_page() {
    // The user's own browser also speaks from 127.0.0.1, and the shared CORS
    // layer answers `Allow-Origin: *`; the `Origin` header is what separates a
    // visited page from the native client.
    let db = setup_db().await;
    let status = status_of(
        db,
        "GET",
        "/contacts",
        Some(LOOPBACK.parse().unwrap()),
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_request_without_a_peer_address_is_refused() {
    // Fail closed: if the connect-info is missing, do not admit the caller.
    let db = setup_db().await;
    let status = status_of(db, "GET", "/contacts", None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── Owner surface: still served to the native client on loopback ─────────────

#[tokio::test]
async fn contacts_list_is_served_to_the_native_client_on_loopback() {
    let db = setup_db().await;
    let status = status_of(
        db,
        "GET",
        "/contacts",
        Some(LOOPBACK.parse().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the local app (loopback, no Origin) must still read its contacts"
    );
}

// ── Public surface: a peer still reaches the catalogue and the receivers ─────

#[tokio::test]
async fn the_catalogue_list_stays_reachable_from_the_lan() {
    let db = setup_db().await;
    insert_public_book(&db, "Public Book").await;
    let status = status_of(db, "GET", "/books", Some(LAN_PEER.parse().unwrap()), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a peer must still browse the shared catalogue"
    );
}

#[tokio::test]
async fn a_single_catalogue_entry_stays_reachable_from_the_lan() {
    let db = setup_db().await;
    let id = insert_public_book(&db, "Public Book").await;
    let status = status_of(
        db,
        "GET",
        &format!("/books/{id}"),
        Some(LAN_PEER.parse().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a public book detail (redacted) must stay reachable to a peer"
    );
}

#[tokio::test]
async fn the_config_handshake_stays_reachable_from_the_lan() {
    let db = setup_db().await;
    let status = status_of(db, "GET", "/config", Some(LAN_PEER.parse().unwrap()), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "peers read /config during the pairing handshake"
    );
}

#[tokio::test]
async fn a_peer_receiver_is_not_blocked_by_the_guard() {
    // `POST /peers/search` is a receiver a remote peer posts to. The guard must
    // not turn it away; the handler may answer 200/400 depending on the body,
    // but never 403 from the loopback guard.
    let db = setup_db().await;
    let status = status_of(
        db,
        "POST",
        "/peers/search",
        Some(LAN_PEER.parse().unwrap()),
        None,
    )
    .await;
    assert_ne!(
        status,
        StatusCode::FORBIDDEN,
        "a peer must still reach the search receiver"
    );
}

#[tokio::test]
async fn a_write_to_the_catalogue_detail_path_is_still_guarded() {
    // `/books/:id` is a split path: GET is public, PUT/DELETE are owner-only.
    // Prove the write side is guarded even though the read side is public.
    let db = setup_db().await;
    let id = insert_public_book(&db, "Public Book").await;
    let status = status_of(
        db,
        "DELETE",
        &format!("/books/{id}"),
        Some(LAN_PEER.parse().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the guard must block the write before the handler runs"
    );
}
