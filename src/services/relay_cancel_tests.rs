//! A borrower cancels a loan request the lender may not have collected yet.
//! The cancellation must follow the request, and asking again must work.

use std::time::{Duration, Instant};

use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::api::peer::{
    RequestAction, cancel_request, delete_outgoing_request, update_request_status,
};
use crate::models::{p2p_request, peer};
use crate::services::relay_test_support::{Device, HOUR, ISBN, Pair, TITLE, message, paired};

const FIRST: &str = "borrower-request-1";
const SECOND: &str = "borrower-request-2";

async fn cancel(p: &Pair, id: &str) {
    let response = delete_outgoing_request(State(p.alice.state.clone()), Path(id.to_string()))
        .await
        .into_response();
    assert_eq!(response.status(), StatusCode::OK);
}

/// The request reached the lender, then the borrower changed her mind.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_removes_a_request_the_lender_already_received() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    p.alice.deposit(&p.bruno, &request).await;
    p.bruno.poll().await;
    assert_eq!(p.bruno.incoming().await.len(), 1);

    cancel(&p, FIRST).await;
    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        Vec::<(Option<String>, String)>::new(),
        "the lender must not keep a cancelled request"
    );
    assert_eq!(p.alice.outgoing_status(FIRST).await, None);
}

/// Without any delay: the false duplicate does not depend on the relay window.
#[tokio::test(flavor = "multi_thread")]
async fn asking_again_after_cancelling_is_not_a_duplicate() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let first = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    p.alice.deposit(&p.bruno, &first).await;
    cancel(&p, FIRST).await;
    let second = p.alice.ask_for_book(p.bruno_on_alice, SECOND, 0).await;
    p.alice.deposit(&p.bruno, &second).await;

    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(SECOND.to_string()), "pending".to_string())],
        "only the live request remains, and it is not refused as a duplicate"
    );
}

/// The whole story while the lender is offline for hours.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_and_asking_again_while_the_lender_is_offline() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let first = p
        .alice
        .ask_for_book(p.bruno_on_alice, FIRST, 2 * HOUR)
        .await;
    p.alice.deposit(&p.bruno, &first).await;
    cancel(&p, FIRST).await;
    let second = p.alice.ask_for_book(p.bruno_on_alice, SECOND, HOUR).await;
    p.alice.deposit(&p.bruno, &second).await;

    p.bruno.poll().await;
    p.alice.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(SECOND.to_string()), "pending".to_string())],
        "the lender sees only the live request"
    );
    assert_eq!(p.alice.outgoing_status(FIRST).await, None);
    assert_eq!(
        p.alice.outgoing_status(SECOND).await.as_deref(),
        Some("pending"),
        "the borrower is not told she was refused"
    );
    assert!(
        !p.alice
            .notifications()
            .await
            .contains(&"borrow_rejected".to_string()),
        "no false refusal"
    );
}

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

async fn remove_lender_mailbox(p: &Pair) {
    let row = peer::Entity::find_by_id(p.bruno_on_alice)
        .one(&p.alice.db)
        .await
        .expect("query")
        .expect("peer");
    let mut active: peer::ActiveModel = row.into();
    active.relay_write_token = Set(None);
    active.update(&p.alice.db).await.expect("update peer");
}

async fn answering_lender(status: u16) -> MockServer {
    let lender = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/e2ee/message"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({})))
        .mount(&lender)
        .await;
    lender
}

/// Requests a lender received, waiting a little for the expected number: what goes
/// direct leaves in the background.
async fn requests_received(lender: &MockServer, expected: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let received = lender
            .received_requests()
            .await
            .expect("recorded requests")
            .len();
        if received >= expected || Instant::now() > deadline {
            return received;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The lender is back and reachable, but has not collected its mailbox yet. A
/// direct cancellation would arrive first and find nothing to cancel.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancellation_stays_behind_the_request_when_the_lender_is_reachable() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, FIRST, 2 * HOUR)
        .await;
    p.alice.deposit(&p.bruno, &request).await;
    let lender = MockServer::start().await;
    set_lender_url(&p, &lender.uri()).await;

    cancel(&p, FIRST).await;
    p.bruno.poll().await;

    assert_eq!(p.hub.deposited(&p.bruno), 2, "request, then cancellation");
    assert_eq!(
        p.bruno.incoming().await,
        Vec::<(Option<String>, String)>::new()
    );
}

/// The borrower may hold a mailbox the lender no longer reads, which the hub still
/// accepts. The copy sent directly is what reaches a lender in that case.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancellation_also_goes_direct_in_case_the_mailbox_is_no_longer_read() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let lender = answering_lender(200).await;
    set_lender_url(&p, &lender.uri()).await;

    cancel(&p, FIRST).await;

    assert_eq!(p.hub.deposited(&p.bruno), 1);
    assert_eq!(requests_received(&lender, 1).await, 1);
}

/// A lender on the previous version answers 404 to the copy sent directly. That
/// says nothing about whether it can be reached.
#[tokio::test(flavor = "multi_thread")]
async fn an_older_lender_answering_404_is_not_marked_unreachable() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let lender = answering_lender(404).await;
    set_lender_url(&p, &lender.uri()).await;

    cancel(&p, FIRST).await;

    assert_eq!(requests_received(&lender, 1).await, 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!p.alice.state.is_peer_direct_unreachable(p.bruno_on_alice));
    assert_eq!(
        p.hub.deposited(&p.bruno),
        1,
        "no second copy in the mailbox"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lender_known_to_be_unreachable_only_gets_the_mailbox_copy() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let lender = answering_lender(200).await;
    set_lender_url(&p, &lender.uri()).await;
    p.alice.state.mark_peer_direct_failed(p.bruno_on_alice);

    cancel(&p, FIRST).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests_received(&lender, 0).await, 0);
    assert_eq!(p.hub.deposited(&p.bruno), 1);
}

/// The screen waits on this call, and only the deposit has to be in order.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_does_not_wait_for_a_slow_direct_route() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let lender = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/e2ee/message"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(Duration::from_millis(2500)),
        )
        .mount(&lender)
        .await;
    set_lender_url(&p, &lender.uri()).await;
    remove_lender_mailbox(&p).await;

    let started = Instant::now();
    cancel(&p, FIRST).await;

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
}

/// No relay credentials: the direct route is all there is.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancellation_goes_direct_to_a_lender_without_a_mailbox() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let lender = answering_lender(200).await;
    set_lender_url(&p, &lender.uri()).await;
    remove_lender_mailbox(&p).await;

    cancel(&p, FIRST).await;

    assert_eq!(requests_received(&lender, 1).await, 1);
    assert_eq!(p.hub.deposited(&p.bruno), 0);
}

/// The lender has new keys: a sealed cancellation could never be opened.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancellation_is_not_deposited_for_keys_the_lender_left() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let pinned = p
        .alice
        .peer_row(p.bruno_on_alice)
        .await
        .expect("peer")
        .x25519_public_key
        .expect("keys");
    p.alice
        .state
        .mark_peer_keys_changed(p.bruno_on_alice, &pinned);

    cancel(&p, FIRST).await;

    assert_eq!(p.alice.outgoing_status(FIRST).await, None, "still deleted");
    assert_eq!(p.hub.deposited(&p.bruno), 0);
}

/// Dismissing an answered request from the history tells nobody anything, and
/// takes no slot in a mailbox capped at 100 messages.
#[tokio::test(flavor = "multi_thread")]
async fn dismissing_an_answered_request_sends_nothing() {
    let p = paired().await;
    let _ = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    let refusal = message(
        "status_update",
        json!({ "loan_id": FIRST, "status": "rejected" }),
        0,
    );
    p.bruno.deposit(&p.alice, &refusal).await;
    p.alice.poll().await;
    assert_eq!(
        p.alice.outgoing_status(FIRST).await.as_deref(),
        Some("rejected")
    );

    cancel(&p, FIRST).await;

    assert_eq!(p.alice.outgoing_status(FIRST).await, None, "dismissed");
    assert_eq!(p.hub.deposited(&p.bruno), 0);
}

/// Known limit: once the lender accepted, the loan stands.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_after_the_lender_accepted_leaves_the_loan() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    p.alice.deposit(&p.bruno, &request).await;
    p.bruno.poll().await;
    let incoming_id = p.bruno.incoming_id(FIRST).await;
    let response = update_request_status(
        State(p.bruno.state.clone()),
        Path(incoming_id),
        Json(RequestAction {
            status: "accepted".to_string(),
        }),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    cancel(&p, FIRST).await;
    p.bruno.poll().await;
    p.alice.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(FIRST.to_string()), "accepted".to_string())]
    );
    assert_eq!(p.bruno.loans().await, 1);
    assert_eq!(p.alice.outgoing_status(FIRST).await, None);
    assert_eq!(p.alice.borrowed_copies().await, 0, "confirmation ignored");
}

#[tokio::test(flavor = "multi_thread")]
async fn another_peer_cannot_cancel_a_request() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let carol = Device::new(&p.hub).await;
    carol.knows(&p.bruno).await;
    p.bruno.knows(&carol).await;
    let request = p.alice.ask_for_book(p.bruno_on_alice, FIRST, 0).await;
    p.alice.deposit(&p.bruno, &request).await;
    p.bruno.poll().await;

    let forged = message(
        "status_update",
        json!({ "loan_id": FIRST, "status": "cancelled" }),
        0,
    );
    carol.deposit(&p.bruno, &forged).await;
    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(FIRST.to_string()), "pending".to_string())]
    );
}

/// A peer without E2EE cancels over HTTP, naming its own request id: the lender
/// stored that id as `requester_request_id`, under an id of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_plaintext_cancel_finds_the_request_by_the_borrower_id() {
    let db = crate::db::init_db("sqlite::memory:")
        .await
        .expect("init db");
    let now = chrono::Utc::now().to_rfc3339();
    let alice_uuid = uuid::Uuid::new_v4().to_string();
    let alice = peer::ActiveModel {
        name: Set("Alice".to_string()),
        url: Set("http://alice.local:8000".to_string()),
        library_uuid: Set(Some(alice_uuid.clone())),
        connection_status: Set("accepted".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert peer");
    p2p_request::ActiveModel {
        id: Set("lender-own-id".to_string()),
        from_peer_id: Set(alice.id),
        book_isbn: Set(ISBN.to_string()),
        book_title: Set(TITLE.to_string()),
        status: Set("pending".to_string()),
        requester_request_id: Set(Some(FIRST.to_string())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        book_id: Set(None),
    }
    .insert(&db)
    .await
    .expect("insert request");

    let response = cancel_request(
        State(db.clone()),
        Path(FIRST.to_string()),
        Some(Json(json!({ "library_uuid": alice_uuid }))),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        p2p_request::Entity::find_by_id("lender-own-id")
            .one(&db)
            .await
            .expect("query")
            .is_none(),
        "answered 200 yet the request is still there"
    );
}
