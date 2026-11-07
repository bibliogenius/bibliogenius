//! The relay exists so that a message can wait for an offline recipient.
//! These tests run the real poller on both devices, with messages sealed long
//! before they are collected, and check the outcome on each side.

use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

use crate::api::e2ee::receive_encrypted_message;
use crate::api::peer::{RequestAction, update_request_status};
use crate::crypto::envelope::ClearMessage;
use crate::services::relay_test_support::{DAY, Device, HOUR, MINUTE, Pair, message, paired};

const BORROWER_REQUEST: &str = "borrower-request-1";

/// Deliver a response naming a new library name: the peer's row tells whether
/// the poller handled it.
async fn deliver_rename(p: &Pair, age_secs: i64) -> String {
    let response = message(
        "library_manifest_response",
        json!({ "library_name": "Renamed" }),
        age_secs,
    );
    p.alice.deposit(&p.bruno, &response).await;
    p.bruno.poll().await;
    p.bruno
        .peer_row(p.alice_on_bruno)
        .await
        .expect("peer row")
        .name
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_collected_right_away_is_handled() {
    let p = paired().await;

    assert_eq!(deliver_rename(&p, 0).await, "Renamed");
    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_collected_ten_minutes_later_is_not_lost() {
    let p = paired().await;

    let name = deliver_rename(&p, 10 * MINUTE).await;

    assert_eq!(
        name, "Renamed",
        "lost: never handled yet deleted from the mailbox"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loan_request_collected_two_hours_later_waits_for_the_lender() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;
    p.alice.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "pending".to_string())],
        "the lender must see the request"
    );
    assert_eq!(
        p.bruno.notifications().await,
        vec!["borrow_request".to_string()],
        "the lender must be told"
    );
    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged once handled");
    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("pending"),
        "the borrower keeps waiting for an answer"
    );
    assert_eq!(p.alice.borrowed_copies().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_loan_request_is_not_auto_approved() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    p.bruno.enable_auto_approve().await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;
    p.alice.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "pending".to_string())],
        "nobody is left waiting for an automatic answer"
    );
    assert_eq!(
        p.bruno.loans().await,
        0,
        "no loan the borrower never hears of"
    );
    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("pending")
    );
    assert_eq!(p.alice.borrowed_copies().await, 0);
}

/// A sender without a relay config of its own attaches no reply fields.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_loan_request_without_reply_fields_is_not_auto_approved() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    p.bruno.enable_auto_approve().await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    let request = ClearMessage {
        reply_to_mailbox: None,
        reply_to_write_token: None,
        ..request
    };
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "pending".to_string())]
    );
    assert_eq!(p.bruno.loans().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_refusal_without_reply_fields_uses_the_known_mailbox() {
    let p = paired().await;
    p.bruno.add_book("lent").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    let request = ClearMessage {
        reply_to_mailbox: None,
        reply_to_write_token: None,
        ..request
    };
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;
    p.alice.poll().await;

    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("rejected")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_loan_request_is_still_auto_approved() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    p.bruno.enable_auto_approve().await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 0)
        .await;
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "accepted".to_string())]
    );
    assert_eq!(p.bruno.loans().await, 1);
    assert_eq!(
        p.hub.deposited(&p.alice),
        1,
        "the synchronous answer goes back to the waiting borrower"
    );
}

/// The borrower waits 90 seconds for the synchronous answer: approving past that
/// would lend a book it never hears about.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_near_the_end_of_the_wait_is_not_auto_approved() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    p.bruno.enable_auto_approve().await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 90)
        .await;
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "pending".to_string())]
    );
    assert_eq!(p.bruno.loans().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_the_borrower_still_waits_on_is_auto_approved() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    p.bruno.enable_auto_approve().await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 30)
        .await;
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;

    assert_eq!(p.bruno.loans().await, 1);
    assert_eq!(p.hub.deposited(&p.alice), 1, "the answer is on its way");
}

#[tokio::test(flavor = "multi_thread")]
async fn accepting_a_late_request_reaches_the_borrower() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    p.alice.deposit(&p.bruno, &request).await;
    p.bruno.poll().await;

    let incoming_id = p.bruno.incoming_id(BORROWER_REQUEST).await;
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
    p.alice.poll().await;

    assert_eq!(p.bruno.loans().await, 1);
    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("accepted")
    );
    assert_eq!(
        p.alice.borrowed_copies().await,
        1,
        "the book is on her list"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_automatic_rejection_reaches_the_borrower() {
    let p = paired().await;
    p.bruno.add_book("lent").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;
    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "rejected".to_string())]
    );
    p.alice.poll().await;

    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("rejected"),
        "the borrower must not wait forever for a refusal"
    );
    assert_eq!(
        p.alice.notifications().await,
        vec!["borrow_rejected".to_string()]
    );
    assert_eq!(p.hub.waiting(&p.alice), 0, "acknowledged once handled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_loan_confirmation_creates_the_borrowed_copy() {
    let p = paired().await;
    let _ = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    let confirmation = message(
        "loan_confirmation",
        json!({
            "title": "Le Livre",
            "isbn": "9782070360024",
            "lender_name": "Bruno",
            "due_date": "2027-01-01",
            "request_id": "lender-request-1",
            "requester_request_id": BORROWER_REQUEST,
        }),
        2 * HOUR,
    );
    p.bruno.deposit(&p.alice, &confirmation).await;

    p.alice.poll().await;

    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("accepted")
    );
    assert_eq!(p.alice.borrowed_copies().await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_loan_confirmation_for_a_cancelled_request_is_ignored() {
    let p = paired().await;
    let confirmation = message(
        "loan_confirmation",
        json!({
            "title": "Le Livre",
            "isbn": "9782070360024",
            "lender_name": "Bruno",
            "due_date": "2027-01-01",
            "request_id": "lender-request-1",
            "requester_request_id": "a-request-she-cancelled",
        }),
        2 * HOUR,
    );
    p.bruno.deposit(&p.alice, &confirmation).await;

    p.alice.poll().await;

    assert_eq!(p.alice.borrowed_copies().await, 0);
    assert_eq!(p.hub.waiting(&p.alice), 0, "acknowledged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_loan_offer_creates_the_borrowed_copy() {
    let p = paired().await;
    let offer = message(
        "loan_offer",
        json!({
            "title": "Le Livre",
            "isbn": "9782070360024",
            "lender_name": "Bruno",
            "due_date": "2027-01-01",
            "request_id": "lender-request-1",
        }),
        2 * HOUR,
    );
    p.bruno.deposit(&p.alice, &offer).await;

    p.alice.poll().await;

    assert_eq!(p.alice.borrowed_copies().await, 1);
    assert_eq!(
        p.alice.notifications().await,
        vec!["loan_offered".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_status_update_reaches_the_borrower() {
    let p = paired().await;
    let _ = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    let update = message(
        "status_update",
        json!({ "loan_id": BORROWER_REQUEST, "status": "rejected" }),
        2 * HOUR,
    );
    p.bruno.deposit(&p.alice, &update).await;

    p.alice.poll().await;

    assert_eq!(
        p.alice.outgoing_status(BORROWER_REQUEST).await.as_deref(),
        Some("rejected")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_disconnect_removes_the_peer() {
    let p = paired().await;
    let disconnect = message("peer_disconnect", json!({}), 2 * HOUR);
    p.bruno.deposit(&p.alice, &disconnect).await;

    p.alice.poll().await;

    assert!(
        p.alice.peer_row(p.bruno_on_alice).await.is_none(),
        "a peer that left must not linger"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn late_relay_credentials_are_applied() {
    let p = paired().await;
    let update = message(
        "relay_credential_update",
        json!({
            "relay_url": "https://hub.example",
            "mailbox_id": "the-new-mailbox",
            "write_token": "abcdef",
        }),
        2 * HOUR,
    );
    p.bruno.deposit(&p.alice, &update).await;

    p.alice.poll().await;

    let row = p.alice.peer_row(p.bruno_on_alice).await.expect("peer row");
    assert_eq!(row.mailbox_id.as_deref(), Some("the-new-mailbox"));
}

/// The requester gave up long ago: answering would only fill its mailbox.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_request_nobody_waits_for_is_dropped_unanswered() {
    let p = paired().await;
    for message_type in [
        "library_manifest_request",
        "library_page_request",
        "library_search_request",
        "search_request",
        "book_sync_request",
        "catalog_delta_request",
        "avatar_sync_request",
        "public_stats_request",
    ] {
        let request = p.alice.request(message_type, json!({}), 10 * MINUTE);
        p.alice.deposit(&p.bruno, &request).await;
    }

    p.bruno.poll().await;

    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged");
    assert_eq!(p.hub.deposited(&p.alice), 0, "nothing sent back");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_request_is_still_answered() {
    let p = paired().await;
    let request = p.alice.request("library_manifest_request", json!({}), 0);
    p.alice.deposit(&p.bruno, &request).await;

    p.bruno.poll().await;

    assert_eq!(p.hub.deposited(&p.alice), 1, "the answer is on its way");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_late_message_is_handled_only_once() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 2 * HOUR)
        .await;
    let envelope = p.alice.seal(&p.bruno, &request);
    p.hub.deposit_envelope(&p.bruno, &envelope);
    p.bruno.poll().await;

    p.hub.deposit_envelope(&p.bruno, &envelope);
    p.bruno.poll().await;

    assert_eq!(
        p.bruno.incoming().await,
        vec![(Some(BORROWER_REQUEST.to_string()), "pending".to_string())],
        "handled once, and the replay left no second request"
    );
    assert_eq!(p.hub.waiting(&p.bruno), 0, "the replay is acknowledged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_just_inside_the_relay_window_is_handled() {
    let p = paired().await;

    assert_eq!(deliver_rename(&p, 7 * DAY - MINUTE).await, "Renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_beyond_the_relay_window_is_refused() {
    let p = paired().await;

    assert_eq!(deliver_rename(&p, 7 * DAY + MINUTE).await, "Peer");
    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_slightly_in_the_future_is_handled() {
    let p = paired().await;

    assert_eq!(deliver_rename(&p, -4 * MINUTE).await, "Renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_too_far_in_the_future_is_refused() {
    let p = paired().await;

    assert_eq!(deliver_rename(&p, -6 * MINUTE).await, "Peer");
    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged");
}

/// The direct endpoint has no mailbox to wait in: it keeps the short limit.
#[tokio::test(flavor = "multi_thread")]
async fn the_direct_endpoint_still_refuses_a_message_older_than_five_minutes() {
    let p = paired().await;
    let envelope = p.alice.seal(
        &p.bruno,
        &message("catalog_changed", json!({}), 10 * MINUTE),
    );

    let response = receive_encrypted_message(State(p.bruno.state.clone()), Json(envelope))
        .await
        .into_response();

    assert_eq!(response.status(), StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_from_an_unknown_sender_is_dropped() {
    let p = paired().await;
    let stranger = Device::new(&p.hub).await;
    p.bruno.add_book("available").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 0)
        .await;
    let envelope = stranger.seal(&p.bruno, &request);
    p.hub.deposit_envelope(&p.bruno, &envelope);

    p.bruno.poll().await;

    assert!(p.bruno.incoming().await.is_empty());
    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged, as before");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_does_not_decrypt_is_dropped() {
    let p = paired().await;
    p.bruno.add_book("available").await;
    let request = p
        .alice
        .ask_for_book(p.bruno_on_alice, BORROWER_REQUEST, 0)
        .await;
    let mut envelope = p.alice.seal(&p.bruno, &request);
    envelope.ciphertext[0] ^= 0xff;
    p.hub.deposit_envelope(&p.bruno, &envelope);

    p.bruno.poll().await;

    assert!(p.bruno.incoming().await.is_empty());
    assert_eq!(p.hub.waiting(&p.bruno), 0, "acknowledged, as before");
}
