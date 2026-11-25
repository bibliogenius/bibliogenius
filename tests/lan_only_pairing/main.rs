//! Two installations that never agreed to register on the hub (ADR-073),
//! paired on the same network: everything a user does between them works
//! over the LAN, and the hub does not see a single request.
//!
//! What this file covers, at the level of the core: pairing without relay
//! credentials, reading the other's catalog, a photographed cover through
//! the local cover proxy, a book change seen at the next sync, a loan asked,
//! accepted and returned, a peer gone offline then back. What it does not
//! cover: the FFI call that closes the gate, mDNS on a real Wi-Fi, the
//! consent dialog. Those stay for a check on real devices.
//!
//! Needs the `it-loopback` feature (peers on 127.0.0.1), not Docker:
//!
//!   cargo test --features it-loopback --test lan_only_pairing
//!
//! One process, one gate, one `HUB_URL`: every test is serial.

#[allow(dead_code)]
#[path = "../two_instances_hub/harness.rs"]
mod harness;

use std::time::{Duration, Instant};

use harness::Instance;
use rust_lib_app::models::book::Book;
use rust_lib_app::models::{copy, p2p_outgoing_request, relay_config};
use rust_lib_app::services::book_service;
use rust_lib_app::services::catalog_notification::schedule_catalog_changed_notification;
use rust_lib_app::services::hub_gate;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::{Value, json};
use serial_test::serial;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Long enough for the tasks a change spawns (peer notices) to have left.
const BACKGROUND_SENDS: Duration = Duration::from_millis(800);

/// A hub that answers 200 to everything and remembers what it was asked.
async fn fake_hub() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "messages": [] })))
        .mount(&server)
        .await;
    // SAFETY: every test of this file is serial.
    unsafe { std::env::set_var("HUB_URL", server.uri()) };
    server
}

async fn requests_seen(hub: &MockServer) -> Vec<String> {
    hub.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

/// A small real JPEG on disk, as the app stores a photographed cover: the
/// book's `cover_url` is the absolute path of the file.
fn photographed_cover(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("cover.jpg");
    image::RgbImage::from_pixel(16, 24, image::Rgb([120, 40, 200]))
        .save(&path)
        .expect("write cover");
    path.to_string_lossy().into_owned()
}

async fn call(method: reqwest::Method, url: String, body: Option<Value>) -> (u16, Value) {
    let mut request = reqwest::Client::new().request(method, &url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{url}: {e}"));
    let status = response.status().as_u16();
    let json = response.json().await.unwrap_or(Value::Null);
    (status, json)
}

/// Pulls `peer_id`'s catalog on `reader`, the app's "sync" on a peer.
async fn sync(reader: &Instance, peer_id: i32) -> (u16, Value) {
    call(
        reqwest::Method::POST,
        format!("{}/api/peers/{peer_id}/sync", reader.url()),
        None,
    )
    .await
}

/// What `reader` caches of `peer_id`'s catalog.
async fn peer_books(reader: &Instance, peer_id: i32) -> Vec<Value> {
    let (status, json) = call(
        reqwest::Method::GET,
        format!("{}/api/peers/{peer_id}/books", reader.url()),
        None,
    )
    .await;
    assert_eq!(status, 200, "[{}] peer books: {json}", reader.name);
    json.as_array().cloned().unwrap_or_default()
}

fn find_title<'a>(books: &'a [Value], title: &str) -> Option<&'a Value> {
    books.iter().find(|b| b["title"].as_str() == Some(title))
}

/// Gives `instance` a relay mailbox on `hub`, as a registered installation
/// holds one, without asking the hub (the fake one must stay untouched).
/// Returns the mailbox uuid.
async fn give_a_mailbox(instance: &Instance, hub: &MockServer) -> String {
    let mailbox_uuid = uuid::Uuid::new_v4().to_string();
    relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(hub.uri()),
        mailbox_uuid: Set(mailbox_uuid.clone()),
        read_token: Set("read-token".to_string()),
        write_token: Set("write-token".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
    }
    .insert(&instance.db)
    .await
    .expect("insert relay config");
    mailbox_uuid
}

/// The pairing on the LAN, the borrower scanning the lender. Returns the
/// borrower's row id of the lender.
async fn pair(borrower: &Instance, lender: &Instance, lender_has_a_mailbox: bool) -> i32 {
    let lender_id = borrower.pair_with(lender).await;
    let lender_row = borrower
        .peer_row(lender.node_id())
        .await
        .expect("the borrower stored the lender");
    assert_eq!(lender_row.connection_status, "accepted");
    assert!(lender_row.key_exchange_done, "keys exchanged");
    assert_eq!(
        lender_row.mailbox_id.is_some() && lender_row.relay_write_token.is_some(),
        lender_has_a_mailbox,
        "the lender's relay credentials are kept exactly when it has some"
    );
    let borrower_row = lender
        .peer_row(borrower.node_id())
        .await
        .expect("the lender stored the borrower");
    assert_eq!(borrower_row.connection_status, "accepted");
    lender_id
}

/// A change in the borrower's own library: the core tells its peers, by
/// relay when it holds their mailbox.
async fn borrower_changes_a_book(borrower: &Instance) {
    book_service::create_book(
        &borrower.db,
        Book {
            title: "La Peste".to_string(),
            isbn: Some("9782070360420".to_string()),
            owned: Some(true),
            ..Default::default()
        },
    )
    .await
    .expect("create book on the borrower");
    schedule_catalog_changed_notification(borrower.state.clone());
    tokio::time::sleep(BACKGROUND_SENDS).await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn two_unregistered_devices_pair_share_lend_and_reconnect_without_the_hub() {
    let hub = fake_hub().await;
    // The app's answer for an installation that never agreed.
    hub_gate::set_open(false);
    assert!(hub_gate::refused());

    let mut lender = Instance::start("lender").await;
    let borrower = Instance::start("borrower").await;
    let lender_id = pair(&borrower, &lender, false).await;
    borrower_changes_a_book(&borrower).await;
    share_lend_and_reconnect(&mut lender, &borrower, lender_id).await;

    let seen = requests_seen(&hub).await;
    assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
}

/// The borrower never agreed; the lender is registered, so the pairing
/// handed over its mailbox. The credentials stay inert while the gate is
/// closed (ADR-073, implementation note 8): everything goes over the LAN.
///
/// Only the change notice has an open-gate twin below. The sync and the
/// loan messages try the peer directly first whatever the gate, so their
/// silence here shows they stay on the LAN, not that the gate stopped them.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_mailbox_of_a_registered_peer_stays_unused_while_the_gate_is_closed() {
    let hub = fake_hub().await;
    hub_gate::set_open(false);

    let mut lender = Instance::start("lender").await;
    let borrower = Instance::start("borrower").await;
    give_a_mailbox(&lender, &hub).await;
    let lender_id = pair(&borrower, &lender, true).await;
    borrower_changes_a_book(&borrower).await;
    share_lend_and_reconnect(&mut lender, &borrower, lender_id).await;

    let seen = requests_seen(&hub).await;
    assert!(seen.is_empty(), "the hub was contacted: {seen:?}");
}

/// The twin of the test above with the gate open: the same change notice
/// reaches the lender's mailbox on the hub, so the silence above is the
/// gate's doing, not a path that never sends.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn the_same_change_notice_reaches_the_hub_once_the_gate_is_open() {
    let hub = fake_hub().await;
    hub_gate::set_open(true);

    let lender = Instance::start("lender").await;
    let borrower = Instance::start("borrower").await;
    let mailbox = give_a_mailbox(&lender, &hub).await;
    pair(&borrower, &lender, true).await;
    borrower_changes_a_book(&borrower).await;

    let seen = requests_seen(&hub).await;
    hub_gate::set_open(false);
    let deposit = format!("POST /api/relay/mailbox/{mailbox}/messages");
    assert!(
        seen.contains(&deposit),
        "with the gate open, the change notice was deposited in the lender's mailbox: {seen:?}"
    );
}

/// Catalog, photographed cover, book change, loan cycle, and a peer gone
/// offline then back, on two paired instances.
async fn share_lend_and_reconnect(lender: &mut Instance, borrower: &Instance, lender_id: i32) {
    // 2 and 3. A book with a photographed cover on the lender, seen by the
    // borrower with its cover.
    let covers = tempfile::tempdir().expect("covers dir");
    let book = book_service::create_book(
        &lender.db,
        Book {
            title: "L'Etranger".to_string(),
            isbn: Some("9782070360024".to_string()),
            author: Some("Albert Camus".to_string()),
            cover_url: Some(photographed_cover(&covers)),
            owned: Some(true),
            ..Default::default()
        },
    )
    .await
    .expect("create book");
    let book_id = book.id.clone().expect("book id");
    schedule_catalog_changed_notification(lender.state.clone());
    tokio::time::sleep(BACKGROUND_SENDS).await;

    let (status, answer) = sync(borrower, lender_id).await;
    assert_eq!(status, 200, "first sync: {answer}");
    let books = peer_books(borrower, lender_id).await;
    let seen = find_title(&books, "L'Etranger")
        .unwrap_or_else(|| panic!("the lender's book in the borrower's cache: {books:?}"));
    let cached_cover = seen["cover_url"].as_str().unwrap_or_default().to_string();
    assert!(
        cached_cover.contains(&format!("/api/books/{book_id}/cover")),
        "the cover is addressed on the lender, not on the hub: {cached_cover:?}"
    );

    let cover = reqwest::Client::new()
        .get(format!("{}/api/peers/cover-proxy", borrower.url()))
        .query(&[("peer_url", lender.url()), ("book_id", book_id.clone())])
        .send()
        .await
        .expect("cover proxy");
    assert_eq!(
        cover.status().as_u16(),
        200,
        "cover through the local proxy"
    );
    assert_eq!(
        cover
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/jpeg")
    );
    assert!(!cover.bytes().await.expect("cover bytes").is_empty());

    // 4. A change on the lender, seen at the borrower's next sync.
    book_service::update_book(
        &lender.db,
        &book_id,
        Book {
            title: "L'Etranger (poche)".to_string(),
            ..book.clone()
        },
    )
    .await
    .expect("update book");
    schedule_catalog_changed_notification(lender.state.clone());
    tokio::time::sleep(BACKGROUND_SENDS).await;
    let (status, answer) = sync(borrower, lender_id).await;
    assert_eq!(status, 200, "sync after the change: {answer}");
    let books = peer_books(borrower, lender_id).await;
    assert!(
        find_title(&books, "L'Etranger (poche)").is_some(),
        "the new title reached the borrower: {books:?}"
    );

    // 5. A loan: asked, accepted, returned.
    let (status, answer) = call(
        reqwest::Method::POST,
        format!("{}/api/peers/{lender_id}/request", borrower.url()),
        Some(json!({
            "book_isbn": "9782070360024",
            "book_title": "L'Etranger (poche)",
            "book_id": book_id,
        })),
    )
    .await;
    assert!(status < 300, "loan request sent: {status} {answer}");

    let (status, incoming) = call(
        reqwest::Method::GET,
        format!("{}/api/peers/requests", lender.url()),
        None,
    )
    .await;
    assert_eq!(status, 200, "lender's requests: {incoming}");
    let incoming = incoming
        .as_array()
        .or_else(|| incoming["requests"].as_array())
        .cloned()
        .unwrap_or_default();
    let request = incoming
        .iter()
        .find(|r| r["status"].as_str() == Some("pending"))
        .unwrap_or_else(|| panic!("a pending request on the lender: {incoming:?}"));
    let request_id = request["id"].as_str().expect("request id").to_string();

    let (status, answer) = call(
        reqwest::Method::PUT,
        format!("{}/api/peers/requests/{request_id}", lender.url()),
        Some(json!({ "status": "accepted" })),
    )
    .await;
    assert!(status < 300, "loan accepted: {status} {answer}");
    tokio::time::sleep(BACKGROUND_SENDS).await;

    let outgoing = p2p_outgoing_request::Entity::find()
        .all(&borrower.db)
        .await
        .expect("outgoing requests");
    assert!(
        outgoing.iter().any(|r| r.status == "accepted"),
        "the borrower learnt the loan was accepted: {outgoing:?}"
    );
    let borrowed = copy::Entity::find()
        .all(&borrower.db)
        .await
        .expect("borrower copies");
    let borrowed = borrowed
        .iter()
        .find(|c| c.status == "borrowed")
        .unwrap_or_else(|| panic!("a borrowed copy on the borrower: {borrowed:?}"));

    let (status, answer) = call(
        reqwest::Method::POST,
        format!("{}/api/peers/return_book", borrower.url()),
        Some(json!({ "copy_id": borrowed.id })),
    )
    .await;
    assert_eq!(status, 200, "return: {answer}");
    assert_eq!(
        answer["lender_notified"],
        json!(true),
        "the lender was told on the LAN: {answer}"
    );

    // 7. The lender goes to sleep: the sync fails cleanly, then works again
    // at once when it is back, without waiting for a retry window.
    lender.go_offline();
    let started = Instant::now();
    let (status, _) = sync(borrower, lender_id).await;
    assert!(
        status >= 400,
        "a sync with the lender offline fails: {status}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the failure came quickly: {:?}",
        started.elapsed()
    );
    lender.come_back_online().await;
    let (status, answer) = sync(borrower, lender_id).await;
    assert_eq!(status, 200, "sync right after the lender is back: {answer}");
    tokio::time::sleep(BACKGROUND_SENDS).await;
}
