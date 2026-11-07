//! An offline device comes back to a full mailbox: the 100 messages the hub keeps,
//! of mixed ages and types, collected in one cycle. The hub handles acknowledgements
//! one by one, so this is the heaviest cycle the poller meets after an update.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::services::relay_test_support::{DAY, HOUR, MINUTE, Pair, message, paired};

const PENDING: usize = 30;
const REFUSED: usize = 10;

fn loan_request(p: &Pair, index: usize, age_secs: i64) -> crate::crypto::envelope::ClearMessage {
    p.alice.request(
        "loan_request",
        json!({
            "book_isbn": format!("isbn-{index}"),
            "book_title": format!("Book {index}"),
            "requester_request_id": format!("borrower-request-{index}"),
        }),
        age_secs,
    )
}

/// Fill Bruno's mailbox with exactly 100 messages from Alice.
async fn fill_the_mailbox(p: &Pair) {
    for index in 0..PENDING {
        p.bruno
            .add_titled_book(
                &format!("isbn-{index}"),
                &format!("Book {index}"),
                "available",
            )
            .await;
        let age = 10 * MINUTE + (index as i64) * 5 * HOUR;
        p.alice
            .deposit(&p.bruno, &loan_request(p, index, age))
            .await;
    }
    for index in PENDING..PENDING + REFUSED {
        p.bruno
            .add_titled_book(&format!("isbn-{index}"), &format!("Book {index}"), "lent")
            .await;
        p.alice
            .deposit(&p.bruno, &loan_request(p, index, 3 * HOUR))
            .await;
    }
    for _ in 0..20 {
        let request = p
            .alice
            .request("library_page_request", json!({}), 10 * MINUTE);
        p.alice.deposit(&p.bruno, &request).await;
    }
    for index in 0..15 {
        let age = if index % 2 == 0 { 0 } else { DAY };
        let nudge = message(
            "catalog_changed",
            json!({ "library_uuid": p.alice.uuid }),
            age,
        );
        p.alice.deposit(&p.bruno, &nudge).await;
    }
    for _ in 0..10 {
        let response = message(
            "library_manifest_response",
            json!({ "library_name": "Renamed" }),
            2 * DAY,
        );
        p.alice.deposit(&p.bruno, &response).await;
    }
    for index in 0..5 {
        let update = message(
            "status_update",
            json!({ "loan_id": format!("unknown-{index}"), "status": "returned" }),
            HOUR,
        );
        p.alice.deposit(&p.bruno, &update).await;
    }
    for index in 0..5 {
        let too_old = loan_request(p, 1000 + index, 8 * DAY);
        p.alice.deposit(&p.bruno, &too_old).await;
    }
    for index in 0..5 {
        let mut damaged = p.alice.seal(&p.bruno, &loan_request(p, 2000 + index, 0));
        damaged.ciphertext[0] ^= 0xff;
        p.hub.deposit_envelope(&p.bruno, &damaged);
    }
    assert_eq!(p.hub.waiting(&p.bruno), 100);
}

/// What a full mailbox must leave behind, whatever the number of cycles.
async fn assert_each_message_was_handled_once(p: &Pair) {
    let incoming = p.bruno.incoming().await;
    let count = |status: &str| incoming.iter().filter(|(_, s)| s == status).count();
    assert_eq!(count("pending"), PENDING);
    assert_eq!(count("rejected"), REFUSED);
    assert_eq!(incoming.len(), PENDING + REFUSED);
    let notifications = p.bruno.notifications().await;
    assert_eq!(notifications.len(), PENDING, "one per request to answer");
    assert!(notifications.iter().all(|n| n == "borrow_request"));
    assert_eq!(p.bruno.loans().await, 0);
    assert_eq!(
        p.hub.deposited(&p.alice),
        REFUSED,
        "only the automatic refusals are sent back"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_mailbox_is_handled_and_emptied_in_one_cycle() {
    let p = paired().await;
    fill_the_mailbox(&p).await;

    let started = Instant::now();
    p.bruno.poll().await;
    println!("BURST full mailbox, no latency: {:?}", started.elapsed());

    assert_eq!(p.hub.waiting(&p.bruno), 0, "all acknowledged");
    assert_each_message_was_handled_once(&p).await;

    p.bruno.poll().await;
    assert_each_message_was_handled_once(&p).await;
}

/// Acknowledgements go one at a time: the wait grows with the network round trip.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_mailbox_over_a_slow_network_stays_within_a_reasonable_time() {
    let p = paired().await;
    fill_the_mailbox(&p).await;
    p.hub.set_latency(Duration::from_millis(150));

    let started = Instant::now();
    p.bruno.poll().await;
    let elapsed = started.elapsed();
    println!("BURST full mailbox, 150 ms per request: {elapsed:?}");

    assert_eq!(p.hub.waiting(&p.bruno), 0, "all acknowledged");
    assert_each_message_was_handled_once(&p).await;
    assert!(elapsed < Duration::from_secs(120), "took {elapsed:?}");
}

/// Timer, nudge and manual triggers can all fire while a long cycle runs.
#[tokio::test(flavor = "multi_thread")]
async fn cycles_triggered_during_a_burst_do_not_handle_anything_twice() {
    let p = paired().await;
    fill_the_mailbox(&p).await;
    p.hub.set_latency(Duration::from_millis(20));

    tokio::join!(p.bruno.poll(), p.bruno.poll(), p.bruno.poll());

    assert_eq!(p.hub.waiting(&p.bruno), 0);
    assert_each_message_was_handled_once(&p).await;
}

/// The hub failing on acknowledgements leaves the messages in place. The next cycle
/// meets them as replays: acknowledged, and not handled a second time.
#[tokio::test(flavor = "multi_thread")]
async fn failed_acknowledgements_are_retried_without_handling_again() {
    let p = paired().await;
    fill_the_mailbox(&p).await;

    p.hub.set_acks_failing(true);
    p.bruno.poll().await;
    assert_eq!(p.hub.waiting(&p.bruno), 100, "nothing could be deleted");
    assert_each_message_was_handled_once(&p).await;

    p.hub.set_acks_failing(false);
    p.bruno.poll().await;
    assert_eq!(p.hub.waiting(&p.bruno), 0);
    assert_each_message_was_handled_once(&p).await;
}
