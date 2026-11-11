//! A catalog answered through the relay must fit in one hub blob.
//!
//! The hub refuses any blob above 64 KB with a 413, and the lender's poller
//! acknowledges the request anyway: the answer is lost and the requester only
//! learns it after a 90 s wait. A full `book_sync_response` crosses that limit
//! around 200 books. These tests run the real poller against a fake hub that
//! enforces the same limit.

use serde_json::{Value, json};

use crate::crypto::envelope::ClearMessage;
use crate::services::relay_test_support::{HUB_MAX_BLOB_SIZE, Pair, paired};

/// A library the size of the one that showed the bug.
const LARGE_CATALOG: usize = 576;

/// Alice sends `message_type` to Bruno over the relay, Bruno's poller answers,
/// and the answer Bruno left in Alice's mailbox is returned (if any).
async fn ask_bruno(p: &Pair, message_type: &str, payload: Value) -> Option<ClearMessage> {
    let before = p.hub.envelopes(&p.alice).len();
    let request = p.alice.request(message_type, payload, 0);
    p.alice.deposit(&p.bruno, &request).await;
    p.bruno.poll().await;
    let answers = p.alice.read_from(&p.hub, &p.bruno, before);
    assert!(answers.len() <= 1, "one answer per request");
    answers.into_iter().next()
}

fn books_of(answer: &ClearMessage) -> Vec<Value> {
    answer
        .payload
        .get("books")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_catalog_of_this_size_does_not_fit_in_one_relay_blob() {
    // Guard on the scenario itself: if this ever passes the limit, the other
    // tests no longer prove anything about large catalogs.
    let p = paired().await;
    p.bruno.add_catalog(LARGE_CATALOG).await;

    let payload = crate::api::e2ee::handle_book_sync_request(&p.bruno.db, None).await;
    let envelope = p.bruno.seal(
        &p.alice,
        &crate::services::relay_test_support::message("book_sync_response", payload, 0),
    );
    let blob_bytes = serde_json::to_vec(&envelope).expect("envelope").len();

    assert!(
        blob_bytes > HUB_MAX_BLOB_SIZE,
        "expected the sealed full catalog ({blob_bytes} bytes) above the hub limit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_catalog_still_comes_whole() {
    let p = paired().await;
    p.bruno.add_catalog(20).await;

    let answer = ask_bruno(&p, "book_sync_request", json!({}))
        .await
        .expect("an answer in Alice's mailbox");

    assert_eq!(answer.message_type, "book_sync_response");
    assert_eq!(answer.payload["status"], "updated");
    assert_eq!(books_of(&answer).len(), 20);
    assert!(answer.payload["catalog_hash"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_too_large_gets_an_answer_instead_of_silence() {
    let p = paired().await;
    p.bruno.add_catalog(LARGE_CATALOG).await;

    let answer = ask_bruno(&p, "book_sync_request", json!({})).await;

    assert_eq!(p.hub.rejected(&p.alice), 0, "no deposit refused with a 413");
    assert!(
        answer.is_some(),
        "the requester must get an answer rather than wait 90 s for nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answer_for_a_catalog_too_large_leaves_the_requester_cache_alone() {
    // A requester reads any status other than "unchanged" as a full snapshot:
    // an empty or missing book list would wipe its cache. Without a
    // catalog_hash it does not record a hash it never received the books for.
    let p = paired().await;
    p.bruno.add_catalog(LARGE_CATALOG).await;

    let answer = ask_bruno(&p, "book_sync_request", json!({}))
        .await
        .expect("an answer in Alice's mailbox");

    assert_eq!(answer.message_type, "book_sync_response");
    assert_eq!(answer.payload["status"], "unchanged");
    assert!(
        answer.payload.get("catalog_hash").is_none(),
        "no hash for a catalog that was not sent"
    );
    assert!(answer.payload.get("books").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_requester_already_in_sync_still_gets_the_hash_back() {
    let p = paired().await;
    p.bruno.add_catalog(LARGE_CATALOG).await;
    let hash = crate::models::Book::compute_catalog_hash(&p.bruno.db).await;

    let answer = ask_bruno(&p, "book_sync_request", json!({ "catalog_hash": hash }))
        .await
        .expect("an answer in Alice's mailbox");

    assert_eq!(answer.payload["status"], "unchanged");
    assert_eq!(answer.payload["catalog_hash"], json!(hash));
}

/// One `INSERT` row in the operation log per book, as a bulk import leaves.
async fn log_every_book_inserted(p: &Pair) {
    use crate::models::{book, operation_log};
    use sea_orm::{EntityTrait, Set};

    let now = chrono::Utc::now().to_rfc3339();
    let books = book::Entity::find().all(&p.bruno.db).await.expect("books");
    let rows = books.into_iter().map(|b| operation_log::ActiveModel {
        entity_type: Set("book".to_owned()),
        entity_id: Set(b.id),
        operation: Set("INSERT".to_owned()),
        payload: Set(None),
        source: Set("local".to_owned()),
        status: Set("applied".to_owned()),
        created_at: Set(now.clone()),
        ..Default::default()
    });
    operation_log::Entity::insert_many(rows)
        .exec(&p.bruno.db)
        .await
        .expect("log inserts");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delta_too_large_is_cut_to_fit_and_says_there_is_more() {
    // After a bulk change (metadata fill, import), hundreds of full books sit
    // in the delta window. The requester asks for up to 500 operations.
    let p = paired().await;
    p.bruno.add_catalog(LARGE_CATALOG).await;
    log_every_book_inserted(&p).await;

    let answer = ask_bruno(
        &p,
        "catalog_delta_request",
        json!({ "since": null, "limit": 500 }),
    )
    .await;

    assert_eq!(p.hub.rejected(&p.alice), 0, "no deposit refused with a 413");
    let answer = answer.expect("an answer in Alice's mailbox");
    assert_eq!(answer.message_type, "catalog_delta_response");
    let operations = answer.payload["operations"].as_array().expect("operations");
    assert!(!operations.is_empty());
    assert_eq!(answer.payload["has_more"], true);
    assert_eq!(answer.payload["reset_required"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn following_the_cursor_of_a_cut_delta_delivers_every_book_once() {
    // The requester stores `latest_cursor` and asks again from there: no
    // book may be skipped or sent twice across the shortened windows.
    let p = paired().await;
    p.bruno.add_catalog(LARGE_CATALOG).await;
    log_every_book_inserted(&p).await;

    let mut ids = std::collections::HashSet::new();
    let mut since = Value::Null;
    let mut rounds = 0;
    loop {
        rounds += 1;
        assert!(rounds <= 40, "the cursor must advance");
        let answer = ask_bruno(
            &p,
            "catalog_delta_request",
            json!({ "since": since, "limit": 500 }),
        )
        .await
        .unwrap_or_else(|| panic!("round {rounds} got no answer"));
        for op in answer.payload["operations"].as_array().expect("operations") {
            assert_eq!(op["op"], "upsert");
            let id = op["book"]["id"].as_str().expect("book id").to_string();
            assert!(ids.insert(id), "a book came twice");
        }
        since = answer.payload["latest_cursor"].clone();
        if answer.payload["has_more"] != true {
            break;
        }
    }

    assert!(rounds > 1, "the scenario must need more than one window");
    assert_eq!(ids.len(), LARGE_CATALOG);
    assert_eq!(p.hub.rejected(&p.alice), 0, "every window fits in a blob");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_delta_is_left_whole() {
    let p = paired().await;
    p.bruno.add_catalog(20).await;
    log_every_book_inserted(&p).await;

    let answer = ask_bruno(
        &p,
        "catalog_delta_request",
        json!({ "since": null, "limit": 500 }),
    )
    .await
    .expect("an answer in Alice's mailbox");

    assert_eq!(
        answer.payload["operations"].as_array().expect("ops").len(),
        20
    );
    assert_eq!(answer.payload["has_more"], false);
}
