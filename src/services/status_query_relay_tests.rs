//! A borrower checks on its pending requests every 30 seconds while the loans
//! screen is open. The answer cannot come back through the relay, so asking
//! there only fills the lender's mailbox, where the oldest messages are evicted.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::api::peer::sync_outgoing_requests;
use crate::models::peer;
use crate::services::relay_test_support::{Pair, message, paired};

const REQUEST: &str = "borrower-request-1";

async fn set_lender_url(p: &Pair, url: &str) {
    let row = peer::Entity::find_by_id(p.bruno_on_alice)
        .one(&p.alice.db)
        .await
        .expect("query")
        .expect("peer");
    let mut active: peer::ActiveModel = row.into();
    active.url = Set(url.to_string());
    active.update(&p.alice.db).await.expect("update peer");
}

#[tokio::test(flavor = "multi_thread")]
async fn checking_on_a_request_leaves_nothing_in_an_unreachable_lenders_mailbox() {
    let p = paired().await;
    let request = p.alice.ask_for_book(p.bruno_on_alice, REQUEST, 0).await;
    p.alice.deposit(&p.bruno, &request).await;
    // Nothing listens on this port: the lender is offline.
    set_lender_url(&p, "http://127.0.0.1:1").await;
    let seen_before = p.hub.requests_seen().await;

    let finished = tokio::time::timeout(
        Duration::from_secs(10),
        sync_outgoing_requests(State(p.alice.state.clone())),
    )
    .await;

    assert_eq!(
        p.hub.deposited(&p.bruno) - 1,
        0,
        "a status query was left in the mailbox of an offline lender"
    );
    assert_eq!(
        p.hub.requests_seen().await,
        seen_before,
        "the hub is not involved, not even to refresh the lender's credentials"
    );
    assert!(
        finished.is_ok(),
        "the check waited for an answer that cannot come"
    );
    assert_eq!(
        p.hub.waiting(&p.bruno),
        1,
        "the loan request is still there"
    );
}

/// A failed direct attempt marks the lender unreachable: the next checks skip it
/// until the mark expires, as they did when they could use its mailbox.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_known_to_be_unreachable_is_not_asked_again() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, REQUEST, 0).await;
    let lender = MockServer::start().await;
    set_lender_url(&p, &lender.uri()).await;
    p.alice.state.mark_peer_direct_failed(p.bruno_on_alice);

    let response = sync_outgoing_requests(State(p.alice.state.clone()))
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        lender
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
    assert_eq!(p.hub.deposited(&p.bruno), 0);
}

/// Without a mailbox, the direct route is the only one: it keeps being tried.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_without_a_mailbox_is_asked_even_after_a_failure() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, REQUEST, 0).await;
    let lender = MockServer::start().await;
    set_lender_url(&p, &lender.uri()).await;
    let row = peer::Entity::find_by_id(p.bruno_on_alice)
        .one(&p.alice.db)
        .await
        .expect("query")
        .expect("peer");
    let mut active: peer::ActiveModel = row.into();
    active.relay_write_token = Set(None);
    active.update(&p.alice.db).await.expect("update peer");
    p.alice.state.mark_peer_direct_failed(p.bruno_on_alice);

    let response = sync_outgoing_requests(State(p.alice.state.clone()))
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        lender
            .received_requests()
            .await
            .expect("recorded requests")
            .len(),
        1
    );
}

/// A write token flagged stale leaves no usable mailbox either.
#[tokio::test(flavor = "multi_thread")]
async fn a_lender_with_a_stale_token_is_asked_even_after_a_failure() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, REQUEST, 0).await;
    let lender = MockServer::start().await;
    set_lender_url(&p, &lender.uri()).await;
    let row = peer::Entity::find_by_id(p.bruno_on_alice)
        .one(&p.alice.db)
        .await
        .expect("query")
        .expect("peer");
    let mut active: peer::ActiveModel = row.into();
    active.relay_write_token_invalid_at = Set(Some(chrono::Utc::now().to_rfc3339()));
    active.update(&p.alice.db).await.expect("update peer");
    p.alice.state.mark_peer_direct_failed(p.bruno_on_alice);

    let response = sync_outgoing_requests(State(p.alice.state.clone()))
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        lender
            .received_requests()
            .await
            .expect("recorded requests")
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_answer_to_a_status_check_is_still_applied() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, REQUEST, 0).await;
    let answer = p.bruno.seal(
        &p.alice,
        &message(
            "request_status_response",
            json!({ "requester_request_id": REQUEST, "status": "rejected" }),
            0,
        ),
    );
    let lender = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/e2ee/message"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-e2ee", "true")
                .set_body_bytes(serde_json::to_vec(&answer).expect("serialize")),
        )
        .mount(&lender)
        .await;
    set_lender_url(&p, &lender.uri()).await;

    let response = sync_outgoing_requests(State(p.alice.state.clone()))
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        p.alice.outgoing_status(REQUEST).await.as_deref(),
        Some("rejected")
    );
    assert_eq!(p.hub.deposited(&p.bruno), 0, "the relay was not involved");
}
