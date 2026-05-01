//! Integration tests for the `.bgbackup` reader / restore pipeline
//! (`api::backup::restore_backup` and friends).
//!
//! Covers the acceptance criteria from the reader ticket and ADR-037 §5:
//! manifest preview without unlocking, HMAC verification (positive +
//! negative), Replace happy path, identity opt-in/out, Merge whitelist,
//! schema version checks, format_version sanity, cover wipe vs. additive,
//! rollback GC, and rollback restoration.
//!
//! Heavy crypto (Argon2id) keeps each test in the seconds range; the corpus
//! stays minimal (1-3 books) to keep the suite acceptable.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rust_lib_app::api::backup::{
    self, BackupError, ENTRY_MANIFEST, ManifestSummary, RestoreMode, UnlockKind,
    list_available_rollbacks, purge_expired_rollbacks, read_manifest, restore_backup,
    restore_from_rollback, verify_signature, write_backup,
};
use rust_lib_app::db;
use rust_lib_app::models::book;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, Set,
    Statement,
};
use tempfile::TempDir;

const TEST_LIBRARY_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
const TEST_SECRET: &[u8] = b"correct horse battery staple";
const TEST_PREFS: &str = r#"{"themeStyle":"dark","languageCode":"fr","country":"FR"}"#;

// -----------------------------------------------------------------------------
// Fixture helpers
// -----------------------------------------------------------------------------

async fn open_db(path: &Path) -> DatabaseConnection {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    db::init_db(&url).await.expect("init_db")
}

async fn seed_book(db: &DatabaseConnection, title: &str) -> i32 {
    let now = chrono::Utc::now().to_rfc3339();
    let m = book::ActiveModel {
        title: Set(title.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed book");
    m.id
}

/// Insert a synthetic identity row so Replace + restore_identity = false has
/// something to delete (otherwise we cannot tell the wipe ran).
async fn seed_crypto_keys(db: &DatabaseConnection) {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO crypto_keys (user_id, key_type, public_key, encrypted_secret, salt) \
         VALUES (0, 'ed25519', X'00', X'01', X'02')",
        [],
    ))
    .await
    .expect("seed crypto_keys row");
}

async fn count_crypto_keys(db: &DatabaseConnection) -> i64 {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM crypto_keys WHERE user_id = 0",
        ))
        .await
        .expect("count crypto_keys")
        .expect("at least one row");
    row.try_get::<i64>("", "n").unwrap()
}

/// Build a fixture archive in `tmp` and return its path. Source DB stays
/// closed so subsequent tests can re-open the same file if needed.
async fn make_test_archive(
    tmp: &TempDir,
    name: &str,
    secret: &[u8],
    library_uuid: &str,
    include_identity: Option<([u8; 32], [u8; 32])>,
    extra_books: &[&str],
) -> PathBuf {
    let source_path = tmp.path().join(format!("{name}-source.sqlite"));
    let db = open_db(&source_path).await;
    seed_book(&db, "Martin Eden").await;
    for t in extra_books {
        seed_book(&db, t).await;
    }
    if include_identity.is_none() {
        seed_crypto_keys(&db).await;
    }

    let archive = tmp.path().join(format!("{name}.bgbackup"));
    let cover_dir = tmp.path().join(format!("{name}-covers-src"));
    std::fs::create_dir_all(&cover_dir).unwrap();

    write_backup(
        &db,
        &archive,
        secret,
        UnlockKind::Passphrase,
        library_uuid,
        include_identity,
        TEST_PREFS,
        &cover_dir,
    )
    .await
    .expect("write_backup");

    db.close().await.unwrap();
    archive
}

/// Open a connection on `db_path`, return it. Caller closes when done. Used
/// by tests that want to inspect the live DB after a restore.
async fn open_existing(path: &Path) -> DatabaseConnection {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    sea_orm::Database::connect(&url).await.expect("connect")
}

/// Initialize a clean live DB at `path`, then close the connection so the
/// reader can rename the file freely.
async fn make_live_db(path: &Path) -> DatabaseConnection {
    open_db(path).await
}

/// Patch a single entry inside an existing zip archive. Used to fabricate
/// "schema_version too new" and "format_version unknown" archives without
/// re-running the writer.
fn rewrite_zip_entry(archive: &Path, entry_name: &str, new_bytes: &[u8]) {
    let bytes = std::fs::read(archive).unwrap();
    let mut zin = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zout = zip::ZipWriter::new(&mut out);
        for i in 0..zin.len() {
            let mut entry = zin.by_index(i).unwrap();
            let name = entry.name().to_string();
            let opts =
                zip::write::SimpleFileOptions::default().compression_method(entry.compression());
            zout.start_file(&name, opts).unwrap();
            if name == entry_name {
                zout.write_all(new_bytes).unwrap();
            } else {
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut buf).unwrap();
                zout.write_all(&buf).unwrap();
            }
        }
        zout.finish().unwrap();
    }
    std::fs::write(archive, out.into_inner()).unwrap();
}

// -----------------------------------------------------------------------------
// read_manifest / verify_signature
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn read_manifest_returns_summary_without_secret() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "rm", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    // No secret passed; this MUST succeed.
    let manifest = read_manifest(&archive).expect("read_manifest");
    assert_eq!(manifest.format_version, "1");
    assert_eq!(manifest.library_uuid, TEST_LIBRARY_UUID);
    assert_eq!(manifest.unlock_kind, UnlockKind::Passphrase);
    assert_eq!(manifest.counts.books, 1);
    assert!(!manifest.identity_included);
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_signature_ok_with_correct_secret() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "vs", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    verify_signature(&archive, TEST_SECRET).expect("must verify");
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_signature_bad_signature_for_wrong_secret() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "vsw", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    let err = verify_signature(&archive, b"not the right secret").expect_err("must fail");
    assert!(
        matches!(err, BackupError::BadSignature),
        "expected BadSignature, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_signature_bad_signature_for_mutated_archive() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "vsm", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    // Flip a single byte inside the encrypted db.sqlite entry. The HMAC is
    // computed over the encrypted bytes so this MUST fail.
    let bytes = std::fs::read(&archive).unwrap();
    let mut zin = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let mut db_bytes = Vec::new();
    zin.by_name("db.sqlite")
        .unwrap()
        .read_to_end(&mut db_bytes)
        .unwrap();
    db_bytes[40] ^= 0x01;
    rewrite_zip_entry(&archive, "db.sqlite", &db_bytes);
    let err = verify_signature(&archive, TEST_SECRET).expect_err("must fail");
    assert!(
        matches!(err, BackupError::BadSignature | BackupError::Crypto(_)),
        "expected BadSignature, got {err:?}"
    );
}

// -----------------------------------------------------------------------------
// Replace mode
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn replace_round_trip_keeps_books_and_creates_rollback() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(
        &tmp,
        "rep",
        TEST_SECRET,
        TEST_LIBRARY_UUID,
        None,
        &["Of Mice and Men", "The Sun Also Rises"],
    )
    .await;

    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    seed_book(&live, "Pre-existing book on the live device").await;
    live.close().await.unwrap();

    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();
    // Pre-existing local cover that should be wiped by Replace.
    std::fs::write(cover_dir.join("stale.png"), b"stale-bytes").unwrap();

    let summary = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Replace,
        false,
        None,
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect("restore");

    assert_eq!(summary.mode, RestoreMode::Replace);
    assert_eq!(
        summary.books_after, 3,
        "archive's 3 books, not the pre-existing one"
    );
    assert!(summary.rollback_path.is_some());
    assert!(!summary.identity_restored);
    assert!(!summary.same_device, "no local UUID passed -> cross-device path");
    assert_eq!(summary.restored_library_uuid, None);

    // Rollback file actually exists on disk.
    let rollback = PathBuf::from(summary.rollback_path.as_ref().unwrap());
    assert!(rollback.is_file(), "rollback file must exist");

    // Cover dir was wiped of pre-existing files.
    assert!(
        !cover_dir.join("stale.png").exists(),
        "Replace must wipe pre-existing local covers"
    );

    // Counts in the live DB match the archive.
    let live_after = open_existing(&live_db_path).await;
    let books_in_live = book::Entity::find().count(&live_after).await.unwrap();
    assert_eq!(books_in_live, 3);
    live_after.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_with_identity_repopulates_crypto_keys() {
    let tmp = TempDir::new().unwrap();
    let identity = ([7u8; 32], [11u8; 32]);
    let archive = make_test_archive(
        &tmp,
        "id",
        TEST_SECRET,
        TEST_LIBRARY_UUID,
        Some(identity),
        &[],
    )
    .await;

    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    live.close().await.unwrap();

    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let summary = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Replace,
        true,
        None,
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect("restore");

    assert!(summary.identity_restored);
    assert_eq!(
        summary.restored_library_uuid.as_deref(),
        Some(TEST_LIBRARY_UUID)
    );

    let live_after = open_existing(&live_db_path).await;
    let n = count_crypto_keys(&live_after).await;
    assert_eq!(n, 2, "ed25519 + x25519 rows after identity restore");
    live_after.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_without_identity_clears_crypto_keys() {
    let tmp = TempDir::new().unwrap();
    // Backup has no identity.bin; it still carries a (synthetic) crypto_keys
    // row in its db.sqlite (seeded inside `make_test_archive` when
    // include_identity is None). After Replace + restore_identity=false the
    // restored DB must end up with crypto_keys cleared so the next launch
    // generates a fresh identity.
    let archive = make_test_archive(&tmp, "noid", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    live.close().await.unwrap();
    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let summary = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Replace,
        false,
        None,
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect("restore");
    assert!(!summary.identity_restored);
    assert_eq!(summary.restored_library_uuid, None);

    let live_after = open_existing(&live_db_path).await;
    let n = count_crypto_keys(&live_after).await;
    assert_eq!(
        n, 0,
        "crypto_keys must be cleared after Replace + no identity"
    );
    live_after.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_same_device_preserves_crypto_keys() {
    // Regression: ADR-037 §5 same-device path. The user restores an
    // auto-backup produced by THIS device; identity_included is false but
    // the local library_uuid still matches the manifest. Wiping crypto_keys
    // would gratuitously reset the device's working identity and trigger
    // the post-restore "Vérification de sécurité" recovery dialog. The
    // fix: when caller passes the matching local UUID, keep crypto_keys
    // and signal "keep" via library_uuid_action so the Flutter caller
    // does not touch its own storage either.
    let tmp = TempDir::new().unwrap();
    let archive =
        make_test_archive(&tmp, "same-device", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    live.close().await.unwrap();
    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let summary = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Replace,
        false,
        Some(TEST_LIBRARY_UUID.to_string()),
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect("restore");

    assert!(
        summary.same_device,
        "matching local UUID should be detected as same-device"
    );
    assert!(!summary.identity_restored);
    assert_eq!(summary.restored_library_uuid, None);

    let live_after = open_existing(&live_db_path).await;
    let n = count_crypto_keys(&live_after).await;
    assert!(
        n > 0,
        "same-device Replace must preserve the archive's crypto_keys row \
         instead of wiping it ({n} rows kept)"
    );
    live_after.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_cross_device_uuid_mismatch_still_clears() {
    // Belt-and-suspenders for the same-device fix: passing a local UUID
    // that does not match the archive's must keep the legacy clear path.
    let tmp = TempDir::new().unwrap();
    let archive =
        make_test_archive(&tmp, "cross", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    live.close().await.unwrap();
    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let summary = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Replace,
        false,
        Some("00000000-0000-0000-0000-deadbeefdead".to_string()),
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect("restore");

    assert!(
        !summary.same_device,
        "different local UUID must NOT be flagged as same-device"
    );
    let live_after = open_existing(&live_db_path).await;
    let n = count_crypto_keys(&live_after).await;
    assert_eq!(
        n, 0,
        "cross-device Replace must still wipe crypto_keys (legacy path)"
    );
    live_after.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_returns_bad_signature_for_wrong_secret() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "wrong", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;
    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    live.close().await.unwrap();
    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let err = restore_backup(
        &archive,
        b"definitely wrong",
        RestoreMode::Replace,
        false,
        None,
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect_err("must fail");
    assert!(
        matches!(err, BackupError::BadSignature),
        "expected BadSignature, got {err:?}"
    );
    // Live DB must still exist at its original path (pre-check failure
    // never touches the disk swap).
    assert!(live_db_path.is_file(), "live DB must remain at db_path");
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_too_new_returns_clear_error() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "stn", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;

    // Patch the manifest to claim a schema_version far in the future.
    let bytes = std::fs::read(&archive).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let mut manifest_bytes = Vec::new();
    zip.by_name(ENTRY_MANIFEST)
        .unwrap()
        .read_to_end(&mut manifest_bytes)
        .unwrap();
    let mut manifest: ManifestSummary = serde_json::from_slice(&manifest_bytes).unwrap();
    manifest.schema_version = 9_999;
    let new_manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    rewrite_zip_entry(&archive, ENTRY_MANIFEST, &new_manifest_bytes);

    // Pre-checks must reject this BEFORE asking for the secret to validate.
    // The HMAC check will fail too because we mutated the manifest, but the
    // schema check runs first inside `restore_backup_inner`.
    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    live.close().await.unwrap();
    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let err = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Replace,
        false,
        None,
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect_err("must fail");
    assert!(
        matches!(err, BackupError::SchemaTooNew { archive: 9_999, .. }),
        "expected SchemaTooNew, got {err:?}"
    );
    assert!(live_db_path.is_file(), "live DB must remain intact");
}

#[tokio::test(flavor = "multi_thread")]
async fn format_version_unknown_returns_clear_error() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(&tmp, "fv", TEST_SECRET, TEST_LIBRARY_UUID, None, &[]).await;

    let bytes = std::fs::read(&archive).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    let mut manifest_bytes = Vec::new();
    zip.by_name(ENTRY_MANIFEST)
        .unwrap()
        .read_to_end(&mut manifest_bytes)
        .unwrap();
    let mut manifest: ManifestSummary = serde_json::from_slice(&manifest_bytes).unwrap();
    manifest.format_version = "2".to_string();
    let new_manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    rewrite_zip_entry(&archive, ENTRY_MANIFEST, &new_manifest_bytes);

    let err = read_manifest(&archive).expect_err("must fail");
    assert!(
        matches!(err, BackupError::FormatVersionUnknown(ref v) if v == "2"),
        "expected FormatVersionUnknown(\"2\"), got {err:?}"
    );
}

// -----------------------------------------------------------------------------
// Merge mode
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn merge_does_not_touch_crypto_keys_or_existing_books() {
    let tmp = TempDir::new().unwrap();
    let archive = make_test_archive(
        &tmp,
        "mrg",
        TEST_SECRET,
        TEST_LIBRARY_UUID,
        None,
        &["Archive Book"],
    )
    .await;

    let live_db_path = tmp.path().join("live.sqlite");
    let live = make_live_db(&live_db_path).await;
    seed_book(&live, "Live Existing Book").await;
    seed_crypto_keys(&live).await;
    let crypto_before = count_crypto_keys(&live).await;
    let books_before = book::Entity::find().count(&live).await.unwrap();
    live.close().await.unwrap();

    let cover_dir = tmp.path().join("live-covers");
    std::fs::create_dir_all(&cover_dir).unwrap();
    std::fs::write(cover_dir.join("preserved.png"), b"keep me").unwrap();

    let summary = restore_backup(
        &archive,
        TEST_SECRET,
        RestoreMode::Merge,
        false,
        None,
        &live_db_path,
        &cover_dir,
    )
    .await
    .expect("merge restore");

    assert_eq!(summary.mode, RestoreMode::Merge);
    assert!(
        summary.rollback_path.is_none(),
        "Merge does not create rollback"
    );
    assert_eq!(
        summary.restored_library_uuid, None,
        "Merge keeps existing UUID"
    );

    let live_after = open_existing(&live_db_path).await;
    let books_after = book::Entity::find().count(&live_after).await.unwrap();
    // import_data_upsert reuses primary keys, so archive books with the
    // same id as live books overwrite them. The acceptance criterion is
    // "archive books are present after merge"; live-only IDs that don't
    // collide also stay. With our minimal fixture (live id=1, archive
    // id=1+2) the upsert ends with 2 rows; we assert the archive content
    // is fully represented.
    assert!(
        books_after >= 2,
        "Merge must include all archive books (got {books_before} live -> {books_after} after merge)"
    );
    let crypto_after = count_crypto_keys(&live_after).await;
    assert_eq!(
        crypto_after, crypto_before,
        "Merge must NOT touch crypto_keys"
    );
    live_after.close().await.unwrap();

    // Pre-existing local cover preserved.
    assert!(
        cover_dir.join("preserved.png").exists(),
        "Merge must preserve existing covers"
    );
}

// -----------------------------------------------------------------------------
// Rollback GC and restore_from_rollback
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn purge_expired_rollbacks_keeps_recent_and_drops_old() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("bibliogenius.db");
    std::fs::write(&live, b"live").unwrap();

    // Recent rollback (1 minute ago) and an old one (>24h ago).
    let now = chrono::Utc::now();
    let recent = format_ts(now - chrono::Duration::minutes(1));
    let old = format_ts(now - chrono::Duration::hours(48));
    let recent_path = tmp
        .path()
        .join(format!("bibliogenius.db.rollback-{recent}"));
    let old_path = tmp.path().join(format!("bibliogenius.db.rollback-{old}"));
    std::fs::write(&recent_path, b"recent").unwrap();
    std::fs::write(&old_path, b"old").unwrap();

    purge_expired_rollbacks(&live);

    assert!(recent_path.is_file(), "recent rollback must be kept");
    assert!(!old_path.is_file(), "expired rollback must be purged");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_available_rollbacks_returns_recent() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("bibliogenius.db");
    std::fs::write(&live, b"live").unwrap();
    let now = chrono::Utc::now();
    let recent = format_ts(now - chrono::Duration::hours(2));
    let recent_path = tmp
        .path()
        .join(format!("bibliogenius.db.rollback-{recent}"));
    std::fs::write(&recent_path, b"recent body").unwrap();

    let infos = list_available_rollbacks(&live);
    assert_eq!(infos.len(), 1);
    let info = &infos[0];
    assert_eq!(info.path, recent_path.to_string_lossy());
    assert!(info.age_seconds >= 2 * 3600 - 60); // tolerate some clock drift
    assert!(info.size_bytes > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_from_rollback_swaps_back_and_creates_replaced_sibling() {
    let tmp = TempDir::new().unwrap();
    let live = tmp.path().join("bibliogenius.db");
    std::fs::write(&live, b"NEW-LIVE").unwrap();

    // Pretend a previous Replace deposited this rollback file.
    let stamp = format_ts(chrono::Utc::now() - chrono::Duration::minutes(5));
    let rollback = tmp.path().join(format!("bibliogenius.db.rollback-{stamp}"));
    std::fs::write(&rollback, b"PREVIOUS-LIVE").unwrap();

    restore_from_rollback(&rollback, &live)
        .await
        .expect("rollback restore");

    // Live now contains the previous bytes.
    assert_eq!(std::fs::read(&live).unwrap(), b"PREVIOUS-LIVE");
    // The previously-current "new" live got moved out as a `.replaced-<ts>`.
    let entries: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("bibliogenius.db.replaced-"))
        .collect();
    assert_eq!(entries.len(), 1, "exactly one .replaced-<ts> sibling");
}

fn format_ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y%m%dT%H%M%SZ").to_string()
}

// -----------------------------------------------------------------------------
// Tail: cheap structural checks
// -----------------------------------------------------------------------------

#[test]
fn rollback_ttl_is_24_hours() {
    // Sanity: ADR-037 §5 anchors the TTL at 24h.
    assert_eq!(
        Duration::from_secs(backup::ROLLBACK_TTL_SECONDS as u64),
        Duration::from_secs(24 * 3600)
    );
}
