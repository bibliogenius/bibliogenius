//! End-to-end test for the hub cover DELETE cleanup (Latent 2 fix).
//!
//! Exercises `HubDirectoryService::delete_cover` against a real
//! `wiremock` hub:
//! - 204 No Content on the DELETE route returns `Ok`.
//! - 404 (or any non-2xx) surfaces as `HubDirectoryError::Hub` so the
//!   caller can decide whether to swallow it (book deletion is
//!   best-effort) or log.
//! - The request carries the correct Bearer token and URL shape; a
//!   silent regression on either would leak orphan covers on the hub.
//!
//! Ticket context: `[TECH-DEBT] Unifier la résolution des cover URLs`
//! Latent 2 (orphelins de covers côté hub).

use rust_lib_app::db;
use rust_lib_app::models::book;
use rust_lib_app::services::book_service;
use rust_lib_app::services::hub_directory_service::{HubDirectoryError, HubDirectoryService};
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};
use serial_test::serial;
use wiremock::matchers::{header, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup_db_with_hub_config() -> DatabaseConnection {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    let now = chrono::Utc::now().to_rfc3339();
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "INSERT INTO hub_directory_config \
             (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, created_at, updated_at) \
             VALUES (1, 'my-node', 'tok-abc', 1, 0, 'everyone', 1, '{now}', '{now}')"
        ),
    ))
    .await
    .expect("insert hub_directory_config");
    db
}

/// 204 on DELETE → `Ok(())`, exactly one call, correct URL + auth.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn delete_cover_204_returns_ok() {
    let db = setup_db_with_hub_config().await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/directory/my-node/covers/42$"))
        .and(header("authorization", "Bearer tok-abc"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let result = svc.delete_cover(&db, 42).await;

    assert!(result.is_ok(), "204 must map to Ok, got {result:?}");
    hub.verify().await;
}

/// 404 on DELETE (missing file) → `HubDirectoryError::Hub(404, ...)`.
///
/// This is the non-regression for the "best-effort" contract: callers
/// in `book_service::delete_book` and `api/books::delete_book` log-and-
/// continue on this error. If a future refactor silently swallows the
/// status code, that test breaks here rather than at runtime.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn delete_cover_404_surfaces_hub_error() {
    let db = setup_db_with_hub_config().await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/directory/my-node/covers/7$"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let err = svc
        .delete_cover(&db, 7)
        .await
        .expect_err("non-2xx must bubble up");

    match err {
        HubDirectoryError::Hub(status, _) => assert_eq!(status, 404),
        other => panic!("expected Hub(404, _), got {other:?}"),
    }
    hub.verify().await;
}

/// 401 Unauthorized on DELETE surfaces the same way as 404: a single
/// error variant covers every non-2xx path. Documents that auth
/// failures are not silently retried.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn delete_cover_401_surfaces_hub_error() {
    let db = setup_db_with_hub_config().await;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/directory/my-node/covers/\d+$"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let err = svc
        .delete_cover(&db, 99)
        .await
        .expect_err("401 must bubble up");

    match err {
        HubDirectoryError::Hub(status, _) => assert_eq!(status, 401),
        other => panic!("expected Hub(401, _), got {other:?}"),
    }
    hub.verify().await;
}

/// Non-regression: `book_service::delete_book` must issue a DELETE to
/// the hub after the DB row is gone. This is the end-to-end proof of
/// the Latent 2 fix. If a future refactor drops the
/// `svc.delete_cover(...)` call, wiremock.verify() would fail (0 calls
/// received instead of 1 expected).
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn delete_book_triggers_hub_cleanup() {
    let db = setup_db_with_hub_config().await;

    // Seed a book so the service has something to delete.
    let now = chrono::Utc::now().to_rfc3339();
    let book_id = book::Entity::insert(book::ActiveModel {
        title: Set("Martin Eden".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert book")
    .last_insert_id;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("DELETE"))
        .and(path_regex(format!(
            r"^/api/directory/my-node/covers/{book_id}$"
        )))
        .and(header("authorization", "Bearer tok-abc"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&hub)
        .await;

    book_service::delete_book(&db, book_id)
        .await
        .expect("delete_book should succeed even if hub cover cleanup did");

    // DB row is gone.
    assert!(
        book::Entity::find_by_id(book_id)
            .one(&db)
            .await
            .expect("query")
            .is_none(),
        "book row must be deleted locally"
    );
    // Hub DELETE fired exactly once.
    hub.verify().await;
}

/// Non-regression: when the hub rejects the cleanup (e.g. 500), the
/// local deletion still succeeds. The best-effort contract is what
/// keeps book deletion responsive even when the hub is down.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn delete_book_succeeds_even_when_hub_cleanup_fails() {
    let db = setup_db_with_hub_config().await;

    let now = chrono::Utc::now().to_rfc3339();
    let book_id = book::Entity::insert(book::ActiveModel {
        title: Set("1984".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert book")
    .last_insert_id;

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("DELETE"))
        .and(path_regex(r"^/api/directory/my-node/covers/\d+$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&hub)
        .await;

    book_service::delete_book(&db, book_id)
        .await
        .expect("hub 500 must not fail the local deletion");

    assert!(
        book::Entity::find_by_id(book_id)
            .one(&db)
            .await
            .expect("query")
            .is_none(),
        "book row must be deleted locally regardless of hub outcome"
    );
    hub.verify().await;
}

/// Without a registered hub profile, `delete_cover` must fail with
/// `NotRegistered` *before* reaching the network. This protects the
/// `book_service::delete_book` best-effort call from hitting the hub
/// for libraries that never registered.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn delete_cover_not_registered_short_circuits() {
    // No hub_directory_config row inserted.
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");

    // Even if HUB_URL is set, the absence of config must short-circuit.
    unsafe { std::env::set_var("HUB_URL", "http://localhost:1") };

    let svc = HubDirectoryService::new();
    let err = svc
        .delete_cover(&db, 1)
        .await
        .expect_err("missing config must fail fast");

    assert!(
        matches!(err, HubDirectoryError::NotRegistered),
        "expected NotRegistered, got {err:?}"
    );
}
