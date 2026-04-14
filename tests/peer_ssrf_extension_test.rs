//! SSRF Protection Tests: extended peer-registration check for proxy_search
//! and request_book_by_url.
//!
//! F/F-bis ticket context:
//! - `cover_proxy` is strict: ensure_registered_peer → 403 if unknown.
//! - `proxy_search` and `request_book_by_url` have a legitimate "unsaved
//!   mDNS peer" fallback used by the UI to preview a neighbor's library
//!   before pairing. Strict blocking would regress that UX.
//!
//! Decision (ADR-026): keep the mDNS fallback, but route it through
//! `ensure_registered_peer_or_mdns(db, url, allow_unregistered_lan=true)`
//! which logs a warn on the `ssrf:mdns` tracing target for audit trail.
//! The strict variant (`ensure_registered_peer`) remains unchanged for
//! cover_proxy and future call sites that do not have a pairing-before
//! fallback requirement.
//!
//! These tests verify:
//! 1. Unit behavior of `ensure_registered_peer_or_mdns` across the 3 cases.
//! 2. Backwards compat: `ensure_registered_peer` still returns FORBIDDEN on
//!    unknown URL (no behavior change for cover_proxy).

use rust_lib_app::api::peer::{ensure_registered_peer, ensure_registered_peer_or_mdns};
use rust_lib_app::db;
use rust_lib_app::models::peer;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

async fn setup_db_with_peer(stored_url: &str) -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    let now = chrono::Utc::now().to_rfc3339();
    let peer = peer::ActiveModel {
        name: Set("Test Peer".to_string()),
        url: Set(stored_url.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    peer.insert(&db).await.expect("insert peer");
    db
}

// ── ensure_registered_peer_or_mdns — registered case ──────────────────

#[tokio::test]
async fn or_mdns_registered_exact_match_returns_some() {
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer_or_mdns(&db, "http://192.168.1.100:8000", true).await;
    let peer = result
        .expect("registered peer must not 403")
        .expect("registered peer must yield Some(peer)");
    assert_eq!(peer.url, "http://192.168.1.100:8000");
}

#[tokio::test]
async fn or_mdns_registered_strict_mode_returns_some() {
    // With allow_unregistered_lan=false, registered peers still work.
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer_or_mdns(&db, "http://192.168.1.100:8000", false).await;
    let peer = result.expect("registered peer must not 403").expect("Some");
    assert_eq!(peer.url, "http://192.168.1.100:8000");
}

#[tokio::test]
async fn or_mdns_registered_trailing_slash_tolerance() {
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer_or_mdns(&db, "http://192.168.1.100:8000/", true).await;
    assert!(
        result.expect("no err").is_some(),
        "slash variant must match"
    );
}

// ── ensure_registered_peer_or_mdns — unregistered case ────────────────

#[tokio::test]
async fn or_mdns_unregistered_with_fallback_returns_none() {
    // mDNS fallback: unknown URL is allowed through, but returns None so
    // the caller knows to skip features that require a peer row (e.g.
    // outgoing-request tracking, cache enrichment).
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer_or_mdns(&db, "http://192.168.1.200:8000", true).await;
    assert!(
        matches!(result, Ok(None)),
        "mDNS fallback must return Ok(None), got {:?}",
        result.map(|o| o.map(|p| p.url))
    );
}

#[tokio::test]
async fn or_mdns_unregistered_strict_mode_returns_forbidden() {
    // Without the mDNS flag, unknown URL is blocked (matches strict
    // ensure_registered_peer behavior used by cover_proxy).
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer_or_mdns(&db, "http://192.168.1.200:8000", false).await;
    assert_eq!(
        result.err(),
        Some(axum::http::StatusCode::FORBIDDEN),
        "strict mode must reject unregistered URL with 403"
    );
}

#[tokio::test]
async fn or_mdns_ssrf_router_target_strict_mode_rejected() {
    // Classic SSRF: even with mDNS fallback ALLOWED, the strict variant
    // still blocks router admin pages. The extended helper only opens the
    // door when explicitly asked by the caller.
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer_or_mdns(&db, "http://192.168.1.1", false).await;
    assert_eq!(result.err(), Some(axum::http::StatusCode::FORBIDDEN));
}

// ── Backwards compat: strict ensure_registered_peer unchanged ─────────

#[tokio::test]
async fn strict_wrapper_still_returns_forbidden_on_unknown() {
    // Regression: ensure_registered_peer (used by cover_proxy) must keep
    // its original contract — Err(FORBIDDEN) on unknown URL. cover_proxy
    // cannot afford a permissive fallback because it fetches arbitrary
    // binary payloads (image bytes).
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.200:8000").await;
    assert_eq!(result.err(), Some(axum::http::StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn strict_wrapper_accepts_registered() {
    let db = setup_db_with_peer("http://192.168.1.100:8000").await;
    let result = ensure_registered_peer(&db, "http://192.168.1.100:8000").await;
    assert!(result.is_ok());
}
