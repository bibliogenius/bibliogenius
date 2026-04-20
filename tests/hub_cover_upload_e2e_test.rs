//! End-to-end test for the hub cover upload failure instrumentation.
//!
//! Exercises `HubDirectoryService::process_local_cover_upload` against a
//! real `wiremock` hub:
//! - when the hub returns 500, `books.hub_cover_upload_failed_at` must be
//!   set (so the owner's UI surfaces the warning badge).
//! - when a subsequent retry succeeds, the flag must be cleared (so the
//!   badge disappears without requiring a manual action).
//!
//! This protects the coupling between the upload loop in
//! `api/frb.rs::hub_directory_sync_catalog` and the bookkeeping helpers:
//! `frb.rs` now delegates to `process_local_cover_upload`, so if a future
//! refactor drops the mark/clear calls the end-to-end flow breaks here.

use std::io::{Cursor, Write};
use std::path::PathBuf;

use image::{DynamicImage, ImageFormat, RgbImage};
use rust_lib_app::db;
use rust_lib_app::models::book;
use rust_lib_app::services::hub_directory_service::HubDirectoryService;
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};
use serial_test::serial;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct TempCoverFile {
    path: PathBuf,
}

impl TempCoverFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempCoverFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

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

async fn insert_book(db: &DatabaseConnection, title: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let active = book::ActiveModel {
        title: Set(title.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book::Entity::insert(active)
        .exec(db)
        .await
        .expect("insert book")
        .last_insert_id
}

/// Writes a minimal valid PNG to a temp file so the resize pipeline has
/// something real to read. The resize function re-encodes to JPEG before
/// upload, so the source format doesn't need to match. Returns a guard
/// that cleans up the file on drop.
fn write_tiny_png_to_temp(tag: &str) -> TempCoverFile {
    let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 96, image::Rgb([120, 80, 160])));
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png)
        .expect("encode png");

    let path =
        std::env::temp_dir().join(format!("bg_cover_e2e_{}_{}.png", tag, std::process::id()));
    let mut file = std::fs::File::create(&path).expect("create temp file");
    file.write_all(&bytes.into_inner()).expect("write png");
    TempCoverFile { path }
}

async fn read_failure_flag(db: &DatabaseConnection, book_id: i32) -> Option<String> {
    book::Entity::find_by_id(book_id)
        .one(db)
        .await
        .expect("find")
        .expect("row exists")
        .hub_cover_upload_failed_at
}

// ── Tests ────────────────────────────────────────────────────────────

/// 500 on the hub → failure flag gets populated so the book-details UI
/// can display its warning badge.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn hub_500_sets_failure_flag() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Martin Eden").await;
    let tmp = write_tiny_png_to_temp("500");

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/\d+$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, book_id, tmp.path().to_str().unwrap())
        .await;

    assert!(url.is_none(), "hub 500 must be surfaced as None to caller");
    assert!(
        read_failure_flag(&db, book_id).await.is_some(),
        "failure flag must be set after a 500"
    );
    hub.verify().await;
}

/// Retry succeeds → flag is cleared so the badge disappears.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn retry_success_clears_failure_flag() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Les mouches").await;
    let tmp = write_tiny_png_to_temp("retry");

    // Seed the flag as if a prior attempt had failed.
    HubDirectoryService::mark_hub_cover_upload_failure(&db, book_id).await;
    assert!(read_failure_flag(&db, book_id).await.is_some());

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/\d+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, book_id, tmp.path().to_str().unwrap())
        .await;

    assert!(url.is_some(), "successful upload must return the hub URL");
    assert!(
        read_failure_flag(&db, book_id).await.is_none(),
        "flag must be cleared once the retry succeeds"
    );
    hub.verify().await;
}

/// 401 behaves like any other hub failure: flag set, badge surfaces.
/// Documents the contract: every non-2xx path from `upload_cover` funnels
/// through the same side-effect.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn hub_401_also_sets_failure_flag() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "1984").await;
    let tmp = write_tiny_png_to_temp("401");

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/\d+$"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, book_id, tmp.path().to_str().unwrap())
        .await;

    assert!(url.is_none());
    assert!(read_failure_flag(&db, book_id).await.is_some());
    hub.verify().await;
}
