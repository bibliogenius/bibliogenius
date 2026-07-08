//! Tests for loan-request delivery failures.
//!
//! Covers two regressions found while investigating a silently lost P2P
//! borrow request:
//! 1. A foreign service answering on the peer's host:port (404/405/501)
//!    must trigger the relay fallback, exactly like a network error. Before,
//!    only `E2eeTransportError::Network` fell back, so a squatted port lost
//!    the message with no relay deposit.
//! 2. When delivery definitively fails, the outgoing request row must be
//!    marked "failed" instead of staying "pending" forever (lying Sent tab +
//!    blocked re-request via the duplicate guard).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{p2p_outgoing_request, peer};
use sea_orm::{DatabaseConnection, EntityTrait, Set};
use serde_json::json;
use tower::util::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── Helpers ──────────────────────────────────────────────────────────

async fn setup_state() -> AppState {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    let state = AppState::new(db);
    state
        .identity_service
        .init("test-library-uuid")
        .await
        .expect("identity init");
    state
}

/// Insert an accepted E2EE-capable peer and return the model.
async fn insert_e2ee_peer(
    db: &DatabaseConnection,
    url: &str,
    relay_url: Option<&str>,
    mailbox_id: Option<&str>,
    relay_write_token: Option<&str>,
) -> peer::Model {
    let identity = rust_lib_app::crypto::identity::NodeIdentity::generate();
    let now = chrono::Utc::now().to_rfc3339();
    let model = peer::ActiveModel {
        name: Set("Squatted Peer".to_string()),
        url: Set(url.to_string()),
        public_key: Set(Some(hex::encode(identity.verifying_key().as_bytes()))),
        x25519_public_key: Set(Some(hex::encode(identity.x25519_public_key().as_bytes()))),
        key_exchange_done: Set(true),
        connection_status: Set("accepted".to_string()),
        relay_url: Set(relay_url.map(str::to_string)),
        mailbox_id: Set(mailbox_id.map(str::to_string)),
        relay_write_token: Set(relay_write_token.map(str::to_string)),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let res = peer::Entity::insert(model)
        .exec(db)
        .await
        .expect("insert peer");
    peer::Entity::find_by_id(res.last_insert_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

/// Mount a squatter that answers the E2EE endpoint with `status`
/// (a real peer never returns 404/405/501 on that route).
async fn mount_squatter(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/api/e2ee/message"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

// ── try_send_e2ee fallback eligibility ───────────────────────────────

/// A 501 from a squatted port must be treated like "peer unreachable":
/// the message gets deposited in the peer's relay mailbox.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_server_response_falls_back_to_relay() {
    let state = setup_state().await;

    let squatter = MockServer::start().await;
    mount_squatter(&squatter, 501).await;

    let relay = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/relay/mailbox/box-1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&relay)
        .await;

    let peer_model = insert_e2ee_peer(
        state.db(),
        &squatter.uri(),
        Some(&relay.uri()),
        Some("box-1"),
        Some("write-tok"),
    )
    .await;

    // No my_relay_config row: the deposit is fire-and-forget (no response
    // await loop), which keeps the test fast.
    let result = rust_lib_app::api::peer::try_send_e2ee_with_timeout(
        &state,
        &peer_model,
        "loan_request",
        json!({ "book_isbn": "9780000000001", "book_title": "Test" }),
        std::time::Duration::from_secs(5),
    )
    .await;

    assert!(
        matches!(result, Ok(Some(None))),
        "501 direct response must fall back to a relay deposit, got {result:?}"
    );
    // wiremock verifies expect(1) on drop: the deposit really happened.
}

/// Error codes the real E2EE endpoint can produce after processing the
/// envelope (e.g. 400 replay rejection) must NOT be relayed: the peer may
/// already have handled the message, a deposit could duplicate it.
#[tokio::test(flavor = "multi_thread")]
async fn real_peer_error_does_not_fall_back_to_relay() {
    let state = setup_state().await;

    let squatter = MockServer::start().await;
    mount_squatter(&squatter, 400).await;

    let relay = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/relay/mailbox/box-1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&relay)
        .await;

    let peer_model = insert_e2ee_peer(
        state.db(),
        &squatter.uri(),
        Some(&relay.uri()),
        Some("box-1"),
        Some("write-tok"),
    )
    .await;

    let result = rust_lib_app::api::peer::try_send_e2ee_with_timeout(
        &state,
        &peer_model,
        "loan_request",
        json!({ "book_isbn": "9780000000001", "book_title": "Test" }),
        std::time::Duration::from_secs(5),
    )
    .await;

    assert!(
        result.is_err(),
        "400 from the peer must surface as an error, got {result:?}"
    );
}

// ── Outgoing request row honesty ─────────────────────────────────────

/// When every delivery channel fails, the outgoing row must end up
/// "failed" (not "pending") and a retry must not be blocked with 409.
#[tokio::test(flavor = "multi_thread")]
async fn failed_delivery_marks_outgoing_request_failed_and_allows_retry() {
    let state = setup_state().await;

    let squatter = MockServer::start().await;
    mount_squatter(&squatter, 501).await;

    // Peer without relay credentials: direct fails (501), relay unavailable,
    // plaintext fallback is blocked by SSRF validation (loopback) -> 502.
    let peer_model = insert_e2ee_peer(state.db(), &squatter.uri(), None, None, None).await;

    let app = Router::new()
        .route(
            "/api/peers/request_by_url",
            post(rust_lib_app::api::peer::request_book_by_url),
        )
        .with_state(state.clone());

    let send_request = || async {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/peers/request_by_url")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "peer_url": peer_model.url,
                            "book_isbn": "9780000000002",
                            "book_title": "Lost Book",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    };

    let response = send_request().await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let rows = p2p_outgoing_request::Entity::find()
        .all(state.db())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].status, "failed",
        "undelivered request must not stay pending"
    );

    // The failed row must not trip the duplicate guard (409 already_requested).
    let retry = send_request().await;
    assert_eq!(
        retry.status(),
        StatusCode::BAD_GATEWAY,
        "retry must be attempted (and fail with 502 here), not rejected with 409"
    );
}
