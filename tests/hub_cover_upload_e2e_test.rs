//! End-to-end test for the hub cover upload failure instrumentation.
//!
//! Exercises `HubDirectoryService::process_local_cover_upload` against a
//! real `wiremock` hub:
//! - when the hub returns 500, the device-local failure flag (in `book_local`,
//!   ADR-044) must be set (so the owner's UI surfaces the warning badge).
//! - when a subsequent retry succeeds, the flag must be cleared (so the
//!   badge disappears without requiring a manual action).
//!
//! This protects the coupling between the upload loop in
//! `api/frb/hub_catalog.rs::hub_directory_sync_catalog` and the bookkeeping helpers:
//! `hub_catalog.rs` now delegates to `process_local_cover_upload`, so if a future
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

async fn insert_book(db: &DatabaseConnection, title: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    // `Entity::insert().exec()` does not fire `before_save`, so the uuid PK is
    // not auto-minted here; set it explicitly (the row's id is its uuid now).
    let active = book::ActiveModel {
        id: Set(format!("test-book-{}", title.replace(' ', "-"))),
        title: Set(title.to_string()),
        reading_status: Set("to_read".to_string()),
        owned: Set(true),
        private: Set(false),
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

/// Writes a PNG of the given dimensions straight to `path`. Two different sizes
/// produce files of different length, so a dedup entry keyed on file identity
/// is invalidated whatever the filesystem's mtime resolution happens to be.
fn write_png_at(path: &std::path::Path, width: u32, height: u32) {
    let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(
        width,
        height,
        image::Rgb([120, 80, 160]),
    ));
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png)
        .expect("encode png");
    let mut file = std::fs::File::create(path).expect("create cover file");
    file.write_all(&bytes.into_inner()).expect("write png");
}

async fn read_failure_flag(db: &DatabaseConnection, book_id: &str) -> Option<String> {
    // The flag is device-local: stored in `book_local`, not on the `books`
    // row (ADR-044).
    rust_lib_app::infrastructure::book_local::cover_upload_failed_at(db, book_id)
        .await
        .expect("read flag")
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
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, None, tmp.path().to_str().unwrap())
        .await;

    assert!(url.is_none(), "hub 500 must be surfaced as None to caller");
    assert!(
        read_failure_flag(&db, &book_id).await.is_some(),
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
    HubDirectoryService::mark_hub_cover_upload_failure(&db, &book_id).await;
    assert!(read_failure_flag(&db, &book_id).await.is_some());

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, None, tmp.path().to_str().unwrap())
        .await;

    assert!(url.is_some(), "successful upload must return the hub URL");
    assert!(
        read_failure_flag(&db, &book_id).await.is_none(),
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
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, None, tmp.path().to_str().unwrap())
        .await;

    assert!(url.is_none());
    assert!(read_failure_flag(&db, &book_id).await.is_some());
    hub.verify().await;
}

/// A cover whose stored path points at a dead iOS data container must still be
/// uploaded, and must therefore NOT raise the warning badge.
///
/// `books.cover_url` keeps the absolute path the device had when the user
/// picked the photo. iOS reassigns the app's data-container UUID across some
/// updates, so that prefix rots while the file itself survives under the new
/// container. The Flutter side re-bases on the book id when it renders, so the
/// cover looks perfectly fine in the app; only the hub upload used to read the
/// column raw, fail with ENOENT, and pin a permanent "cover not synced" badge
/// on the book detail sheet.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn stale_container_path_is_rebased_and_uploads_cleanly() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "El Aleph").await;

    // The file lives under the CURRENT covers dir, named `<book_id>.jpg`.
    let covers_dir = std::env::temp_dir().join(format!("bg_covers_{}", std::process::id()));
    std::fs::create_dir_all(&covers_dir).expect("create covers dir");
    let real = write_tiny_png_to_temp("stale");
    let current = covers_dir.join(format!("{book_id}.jpg"));
    std::fs::copy(real.path(), &current).expect("seed current cover");

    // ...while the DB still holds the path of a container that no longer exists.
    let stored = format!(
        "/var/mobile/Containers/Data/Application/DEAD-UUID/Library/Application Support/covers/{book_id}.jpg"
    );
    assert!(
        !std::path::Path::new(&stored).exists(),
        "the stored path must really be dead for this test to mean anything"
    );

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), &stored)
        .await;

    let _ = std::fs::remove_dir_all(&covers_dir);

    assert!(
        url.is_some(),
        "the cover file is on disk under the current container: the upload must succeed"
    );
    assert!(
        read_failure_flag(&db, &book_id).await.is_none(),
        "no warning badge may be raised for a cover that uploaded fine"
    );
    hub.verify().await;
}

/// The negative control for the test above: with no covers directory to
/// re-base onto (server-binary mode) the dead path is read as-is and fails,
/// which is the behaviour the app used to have on every custom cover.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_dead_path_without_a_covers_dir_still_flags_the_book() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Les mouches 2").await;
    let stored = format!(
        "/var/mobile/Containers/Data/Application/DEAD-UUID/Library/Application Support/covers/{book_id}.jpg"
    );

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, None, &stored)
        .await;

    assert!(url.is_none(), "an unreadable cover cannot be uploaded");
    assert!(
        read_failure_flag(&db, &book_id).await.is_some(),
        "a genuinely unreadable cover must still raise the badge"
    );
}

/// A `cover_url` carrying a relative segment must be refused before the file is
/// opened, and must never reach the hub.
///
/// The column is replicated raw across devices (ADR-011), so its value is not
/// necessarily something this device wrote: a compromised paired device can put
/// an arbitrary path there. The peer-facing endpoint already rejects `..`; this
/// pins the same guard on the upload side, which otherwise reads whatever the
/// column names whenever the basename does not match `<book_id>.jpg` and POSTs
/// the bytes to the hub.
///
/// The traversal target here is a REAL decodable image, so the pipeline would
/// happily read it and upload it if the guard were dropped: an unreadable
/// target would fail at the decode step and make this test pass for the wrong
/// reason.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_traversal_path_is_refused_before_any_read() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Traversal").await;

    let real = write_tiny_png_to_temp("traversal");
    let dir = real.path().parent().expect("temp dir");
    let name = real.path().file_name().expect("file name");
    // `<tmp>/sub/../<file>` resolves to the very same readable PNG. The
    // intermediate directory has to exist: the kernel walks `..` through real
    // directories, it does not collapse the path lexically.
    let sub = dir.join(format!("bg_traversal_sub_{}", std::process::id()));
    std::fs::create_dir_all(&sub).expect("create intermediate dir");
    let traversal = sub.join("..").join(name);
    assert!(
        std::fs::read(&traversal).is_ok(),
        "the traversal target must really be readable, or the test proves nothing"
    );

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    // Deliberately no mock: the request log below is what proves the bytes
    // never left the device. `verify()` would pass trivially with nothing
    // mounted, and an unmatched request still gets a 404 the caller reports as
    // a plain failure, so neither would discriminate.

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, None, traversal.to_str().unwrap())
        .await;

    let _ = std::fs::remove_dir_all(&sub);

    let seen = hub.received_requests().await.unwrap_or_default();
    assert!(
        seen.is_empty(),
        "the path must be refused before any read or upload, got {} request(s)",
        seen.len()
    );
    assert!(
        url.is_none(),
        "a traversal path must never produce a hub URL"
    );
}

/// An absolute path that names a readable file OUTSIDE the covers directory
/// must never be opened, even though it contains no `..` segment at all.
///
/// This is the half of the class the `..` guard does not reach. `books.cover_url`
/// is replicated raw across devices (ADR-011), so a compromised paired device can
/// store `/etc/hosts` there: no relative segment, perfectly readable, and the
/// bytes would be POSTed to the hub for anyone following the library to fetch.
/// In app mode the stored value is therefore ignored entirely and the path is
/// derived from the book's own identity, the same way `services/cover_sync.rs`
/// builds `covers_dir/<uuid>.jpg`.
///
/// The decoy is a REAL decodable image, so the pipeline would read it, re-encode
/// it and upload it if the derivation were dropped. Pointing at a file that does
/// not decode would make this test pass at the decode step instead, proving
/// nothing about whether the read happened.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_absolute_path_outside_the_covers_dir_is_never_read() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Outside").await;

    // An empty covers dir: this book has no legitimate local cover to upload.
    let covers_dir = std::env::temp_dir().join(format!("bg_covers_outside_{}", std::process::id()));
    std::fs::create_dir_all(&covers_dir).expect("create covers dir");
    assert!(
        !covers_dir.join(format!("{book_id}.jpg")).exists(),
        "the covers dir must hold no cover for this book, or the test proves nothing"
    );

    // The decoy stands in for any absolute path a paired device could plant.
    let decoy = write_tiny_png_to_temp("outside");
    let stored = decoy.path().to_str().expect("utf-8 temp path").to_owned();
    assert!(
        !stored.split(['/', '\\']).any(|seg| seg == ".."),
        "the decoy must carry no traversal segment: the `..` guard is not what is under test"
    );
    assert!(
        std::fs::read(&stored).is_ok(),
        "the decoy must really be readable, or the test proves nothing"
    );

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    // Deliberately no mock mounted: the request log is what proves the bytes
    // never left the device. `verify()` passes trivially with nothing mounted.

    let svc = HubDirectoryService::new();
    let url = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), &stored)
        .await;

    let _ = std::fs::remove_dir_all(&covers_dir);

    let seen = hub.received_requests().await.unwrap_or_default();
    assert!(
        seen.is_empty(),
        "a file outside the covers dir must never be read or uploaded, got {} request(s)",
        seen.len()
    );
    assert!(
        url.is_none(),
        "a cover this device does not actually hold must not produce a hub URL"
    );
}

/// A cover already sent during this run is not sent again, and the caller still
/// gets its hub URL back.
///
/// The second half is the point, not a detail. The catalog builder starts every
/// local cover at `cover_url: None` and fills it FROM THE RETURN VALUE of this
/// call (`api/frb/hub_catalog.rs`), so a dedup that returned `None` on a hit
/// would push a catalog carrying no cover at all and every follower browsing
/// the library would see a placeholder. Skipping the upload has to be invisible
/// to the caller.
///
/// The test drives one service instance twice, which is what production does:
/// `hub_directory_svc()` is a process-wide `OnceLock` singleton.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_unchanged_cover_is_uploaded_once_and_its_url_replayed() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Dedup").await;

    let covers_dir = std::env::temp_dir().join(format!("bg_covers_dedup_{}", std::process::id()));
    std::fs::create_dir_all(&covers_dir).expect("create covers dir");
    let stored = covers_dir.join(format!("{book_id}.jpg"));
    write_png_at(&stored, 64, 96);

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let path = stored.to_str().expect("utf-8 path");
    let first = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), path)
        .await;
    let second = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), path)
        .await;

    let _ = std::fs::remove_dir_all(&covers_dir);

    assert!(first.is_some(), "the first upload must succeed");
    assert_eq!(
        second, first,
        "a cache hit must replay the same hub URL: the pushed catalog is built \
         from this value, and `None` here means followers see no cover"
    );
    assert_eq!(
        hub.received_requests().await.unwrap_or_default().len(),
        1,
        "an unchanged cover must reach the hub exactly once per run"
    );
}

/// The companion guard to the test above: dedup must not outlive the file it
/// describes. Replacing the photo has to send the new bytes.
///
/// This one passes vacuously before the dedup exists (nothing is ever skipped),
/// so it proves nothing on its own. It earns its keep afterwards, as the test
/// that fails if the cache never invalidates.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_replaced_cover_is_uploaded_again() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Replaced").await;

    let covers_dir =
        std::env::temp_dir().join(format!("bg_covers_replaced_{}", std::process::id()));
    std::fs::create_dir_all(&covers_dir).expect("create covers dir");
    let stored = covers_dir.join(format!("{book_id}.jpg"));
    write_png_at(&stored, 64, 96);

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let path = stored.to_str().expect("utf-8 path");
    svc.process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), path)
        .await;

    // The reader picked a new photo: different dimensions, different length.
    write_png_at(&stored, 80, 120);
    let after = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), path)
        .await;

    let _ = std::fs::remove_dir_all(&covers_dir);

    assert!(after.is_some(), "the replacement upload must succeed");
    assert_eq!(
        hub.received_requests().await.unwrap_or_default().len(),
        2,
        "a replaced photo must be sent again, not masked by a stale dedup entry"
    );
}

/// Purging the hub registration drops the remembered uploads.
///
/// The replayed URL carries the node id, and a purge is what precedes getting a
/// new one. Keeping the entries would hand followers URLs pointing at a node
/// that no longer owns the blobs, and no upload would ever correct it for the
/// rest of the run. `hub_directory_purge_config` calls this alongside the
/// failure-flag reset it already did.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn purging_the_registration_forgets_the_uploaded_covers() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Purged").await;

    let covers_dir = std::env::temp_dir().join(format!("bg_covers_purged_{}", std::process::id()));
    std::fs::create_dir_all(&covers_dir).expect("create covers dir");
    let stored = covers_dir.join(format!("{book_id}.jpg"));
    write_png_at(&stored, 64, 96);

    let hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", hub.uri()) };
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&hub)
        .await;

    let svc = HubDirectoryService::new();
    let path = stored.to_str().expect("utf-8 path");
    svc.process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), path)
        .await;

    svc.forget_uploaded_covers();

    let after = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), path)
        .await;

    let _ = std::fs::remove_dir_all(&covers_dir);

    assert!(after.is_some(), "the upload after a purge must succeed");
    assert_eq!(
        hub.received_requests().await.unwrap_or_default().len(),
        2,
        "a purge must not leave the cover masked by a remembered upload"
    );
}

/// A hub change invalidates the remembered upload, and a failure against the
/// new hub must not leave the old entry behind to mask the recovery.
///
/// Reachable, not theoretical: `api/peer/relay_config.rs` drops the directory
/// config whenever the relay's hub URL changes, mid-run, and the
/// re-registration that follows yields a different node id. The remembered URL
/// carries both, so it has to stop being trusted the moment the hub does not
/// match.
///
/// The tail is what the eviction protects. Once the hub comes back, a stale
/// entry would be replayed, the caller would skip clearing the failure flag it
/// believes a previous success already cleared, and the book would keep a
/// "cover not synced" badge for the rest of the run over a cover the hub holds.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_hub_change_invalidates_the_remembered_upload() {
    let db = setup_db_with_hub_config().await;
    let book_id = insert_book(&db, "Moved").await;

    let covers_dir = std::env::temp_dir().join(format!("bg_covers_moved_{}", std::process::id()));
    std::fs::create_dir_all(&covers_dir).expect("create covers dir");
    let stored = covers_dir.join(format!("{book_id}.jpg"));
    write_png_at(&stored, 64, 96);
    let path = stored.to_str().expect("utf-8 path").to_owned();

    let svc = HubDirectoryService::new();

    // The original hub accepts the cover, so it is remembered.
    let first_hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", first_hub.uri()) };
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/directory/my-node/covers/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&first_hub)
        .await;
    svc.process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), &path)
        .await;
    assert!(
        read_failure_flag(&db, &book_id).await.is_none(),
        "the first upload must leave no warning badge"
    );

    // The relay is repointed at another hub, which refuses. The remembered URL
    // belongs to the old hub, so it must not be replayed here.
    let second_hub = MockServer::start().await;
    unsafe { std::env::set_var("HUB_URL", second_hub.uri()) };
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&second_hub)
        .await;
    let refused = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), &path)
        .await;
    assert!(
        refused.is_none(),
        "the new hub refused, so no URL may come back"
    );
    assert_eq!(
        second_hub
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        1,
        "the cover must really be offered to the new hub, not replayed from cache"
    );
    assert!(
        read_failure_flag(&db, &book_id).await.is_some(),
        "a refused upload must raise the warning badge"
    );

    // Back on the original hub. The file never changed, so a surviving entry
    // would be replayed and the badge would never clear again.
    unsafe { std::env::set_var("HUB_URL", first_hub.uri()) };
    let recovered = svc
        .process_local_cover_upload(&db, &book_id, Some(covers_dir.as_path()), &path)
        .await;

    let _ = std::fs::remove_dir_all(&covers_dir);

    assert!(recovered.is_some(), "the recovery upload must succeed");
    assert!(
        read_failure_flag(&db, &book_id).await.is_none(),
        "the badge must clear on recovery: a stale cache entry would skip the \
         clearing and pin it for the rest of the run"
    );
}
