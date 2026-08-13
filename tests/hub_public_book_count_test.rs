//! The public `book_count` advertised on the hub profile must describe the
//! catalog followers actually receive, not the raw `books` row count.
//!
//! Two writers set that number on the hub: the catalog push (which sends
//! `entries.len()`) and the profile upsert. When the profile upsert sent a
//! raw count, it won the race and the remote library header announced books
//! nobody could ever see (wishlist rows, entries with neither ISBN nor
//! title). These tests pin the profile upsert to the catalog rule.

use rust_lib_app::db;
use rust_lib_app::services::hub_directory_service::{HubDirectoryService, RegisterParams};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Seeds a library whose raw row count (7) differs from the number of books
/// that reach the hub catalog (4):
///   - two ordinary owned books with ISBN and title,
///   - one owned book with an ISBN but no title (manually entered, the case
///     that rendered as an empty tile on the peer screen: it IS pushed and
///     MUST be counted),
///   - one owned book with a title but no ISBN,
///   - one wishlist row (`owned = 0`), never pushed,
///   - one owned book with neither ISBN nor title, unusable and never pushed,
///   - one owned book marked `private`, which the user asked to hide from
///     peers: never pushed, and never counted in the public number either.
async fn seed_books(db: &DatabaseConnection) {
    // (uuid, title, isbn, owned, private)
    const ROWS: [(&str, &str, &str, i32, i32); 7] = [
        ("b-1", "Le Mythe de Sisyphe", "9782070612918", 1, 0),
        ("b-2", "Slaughterhouse-Five", "9780140283334", 1, 0),
        ("b-3", "", "9782266320269", 1, 0),
        ("b-4", "Carnet manuscrit", "", 1, 0),
        ("b-5", "Wishlist entry", "9782070360024", 0, 0),
        ("b-6", "", "", 1, 0),
        ("b-7", "Journal intime", "9782070368228", 1, 1),
    ];
    for (uuid, title, isbn, owned, private) in ROWS {
        db.execute(Statement::from_string(
            db.get_database_backend(),
            format!(
                "INSERT INTO books (uuid, title, isbn, owned, private, reading_status, created_at, updated_at) \
                 VALUES ('{uuid}', '{title}', '{isbn}', {owned}, {private}, 'to_read', \
                 '2026-08-01T00:00:00Z', '2026-08-01T00:00:00Z')"
            ),
        ))
        .await
        .expect("insert book");
    }
}

fn base_params() -> RegisterParams {
    RegisterParams {
        node_id: "node-abc".to_string(),
        display_name: "Test Library".to_string(),
        is_listed: true,
        requires_approval: false,
        accept_from: "everyone".to_string(),
        allow_borrowing: true,
        ..Default::default()
    }
}

fn profile_json() -> serde_json::Value {
    serde_json::json!({
        "node_id": "node-abc",
        "display_name": "Test Library",
        "description": null,
        "book_count": 0,
        "location_country": null,
        "requires_approval": false,
        "allow_borrowing": true,
        "last_seen_at": null,
        "write_token": "tok-fresh-abc",
        "view_count": 0,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn profile_upsert_advertises_the_public_catalog_count() {
    let db = setup_test_db().await;
    seed_books(&db).await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path("/api/directory/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(profile_json()))
        .expect(1)
        .mount(&hub)
        .await;

    HubDirectoryService::new()
        .register_or_update(&db, base_params())
        .await
        .expect("register_or_update succeeds");

    let received = hub.received_requests().await.expect("requests recorded");
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("body is JSON");

    assert_eq!(
        body.get("book_count").and_then(|v| v.as_i64()),
        Some(4),
        "profile must announce the 4 books the hub catalog carries, not the 7 rows in `books`",
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn public_catalog_count_applies_the_catalog_eligibility_rule() {
    let db = setup_test_db().await;
    seed_books(&db).await;

    assert_eq!(
        HubDirectoryService::count_public_catalog_books(&db)
            .await
            .expect("count succeeds"),
        4,
    );
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn public_catalog_count_is_zero_on_an_empty_library() {
    let db = setup_test_db().await;

    assert_eq!(
        HubDirectoryService::count_public_catalog_books(&db)
            .await
            .expect("count succeeds"),
        0,
    );
}
