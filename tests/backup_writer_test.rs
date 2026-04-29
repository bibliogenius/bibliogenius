//! Integration tests for the `.bgbackup` writer (`api::backup::write_backup`).
//!
//! Covers the acceptance criteria from the backup writer ticket and ADR-037:
//! manifest round-trip, identity opt-in, HKDF subkey separation, local-only
//! cover detection, HMAC integrity (positive + negative), and a clean error
//! when the output destination cannot be written.
//!
//! Heavy crypto (Argon2id) keeps these tests in the seconds range; we keep
//! the corpus tiny (1-2 books) so the cost stays acceptable.

use std::io::Read;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rust_lib_app::api::backup::{
    self, BackupError, ENTRY_DB, ENTRY_IDENTITY, ENTRY_MANIFEST, ENTRY_PREFS, ENTRY_SIGNATURE,
    HKDF_INFO_AES, HKDF_INFO_HMAC, ManifestSummary, UnlockKind, write_backup,
};
use rust_lib_app::crypto::encryption::derive_key_from_password;
use rust_lib_app::db;
use rust_lib_app::models::book;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use sha2::Sha256;
use tempfile::TempDir;

const TEST_LIBRARY_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
const TEST_SECRET: &[u8] = b"correct horse battery staple";
const TEST_PREFS: &str = r#"{"theme":"dark","language":"fr","country":"FR"}"#;

/// Build a SeaORM connection backed by a real file inside `tmp`. We avoid
/// `:memory:` because `VACUUM INTO` over an in-memory connection is
/// silently no-op'd by the sqlx-sqlite driver in some versions;
/// production always runs against a file-backed DB.
async fn setup_test_db(tmp: &TempDir) -> DatabaseConnection {
    let db_path = tmp.path().join("source.sqlite");
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    db::init_db(&url).await.expect("init_db")
}

async fn seed_minimal(db: &DatabaseConnection) {
    let now = chrono::Utc::now().to_rfc3339();
    book::ActiveModel {
        title: Set("Martin Eden".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed book");
}

/// Read a single ZIP entry by name. Returns the raw bytes (still encrypted
/// for everything except `manifest.json` and `signature`).
fn read_zip_entry(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("open zip");
    let mut f = zip.by_name(name).ok()?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf).expect("read zip entry");
    Some(buf)
}

fn list_zip_entries(archive: &[u8]) -> Vec<String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("open zip");
    (0..zip.len())
        .map(|i| zip.by_index(i).expect("entry").name().to_string())
        .collect()
}

fn parse_manifest(archive: &[u8]) -> ManifestSummary {
    let bytes = read_zip_entry(archive, ENTRY_MANIFEST).expect("manifest entry");
    serde_json::from_slice(&bytes).expect("parse manifest")
}

#[tokio::test]
async fn round_trip_manifest_carries_counts_and_metadata() {
    let tmp = TempDir::new().unwrap();
    let db = setup_test_db(&tmp).await;
    seed_minimal(&db).await;

    let out = tmp.path().join("backup.bgbackup");
    let cover_dir = tmp.path().join("covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let summary = write_backup(
        &db,
        &out,
        TEST_SECRET,
        UnlockKind::Passphrase,
        TEST_LIBRARY_UUID,
        None,
        TEST_PREFS,
        &cover_dir,
    )
    .await
    .expect("write_backup");

    assert_eq!(summary.archive_path, out);
    assert!(summary.archive_size_bytes > 0);
    assert_eq!(
        summary.archive_size_bytes,
        std::fs::metadata(&out).unwrap().len()
    );

    let archive = std::fs::read(&out).unwrap();
    let m = parse_manifest(&archive);
    assert_eq!(m.format_version, "1");
    assert_eq!(m.library_uuid, TEST_LIBRARY_UUID);
    assert!(!m.identity_included);
    assert_eq!(m.unlock_kind, UnlockKind::Passphrase);
    assert_eq!(m.counts.books, 1);
    assert_eq!(m.counts.copies, 0);
    assert_eq!(m.counts.contacts, 0);
    assert!(m.argon2.m_cost >= 65536);
    assert_eq!(B64.decode(&m.argon2.salt_b64).unwrap().len(), 32);
    assert_eq!(m.app_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(m.db_sha256.len(), 64); // hex sha256

    // Sanity: required entries present, no identity entry.
    let entries = list_zip_entries(&archive);
    assert!(entries.iter().any(|e| e == ENTRY_MANIFEST));
    assert!(entries.iter().any(|e| e == ENTRY_DB));
    assert!(entries.iter().any(|e| e == ENTRY_PREFS));
    assert!(entries.iter().any(|e| e == ENTRY_SIGNATURE));
    assert!(!entries.iter().any(|e| e == ENTRY_IDENTITY));
}

#[tokio::test]
async fn identity_opt_in_controls_entry_presence() {
    let tmp = TempDir::new().unwrap();
    let db = setup_test_db(&tmp).await;
    seed_minimal(&db).await;
    let cover_dir = tmp.path().join("covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    let with_id = tmp.path().join("with-identity.bgbackup");
    let without_id = tmp.path().join("without-identity.bgbackup");

    let identity_bytes = ([7u8; 32], [11u8; 32]);

    write_backup(
        &db,
        &with_id,
        TEST_SECRET,
        UnlockKind::Passphrase,
        TEST_LIBRARY_UUID,
        Some(identity_bytes),
        TEST_PREFS,
        &cover_dir,
    )
    .await
    .expect("with identity");

    write_backup(
        &db,
        &without_id,
        TEST_SECRET,
        UnlockKind::Passphrase,
        TEST_LIBRARY_UUID,
        None,
        TEST_PREFS,
        &cover_dir,
    )
    .await
    .expect("without identity");

    let bytes_with = std::fs::read(&with_id).unwrap();
    let bytes_without = std::fs::read(&without_id).unwrap();

    assert!(parse_manifest(&bytes_with).identity_included);
    assert!(!parse_manifest(&bytes_without).identity_included);

    let entries_with = list_zip_entries(&bytes_with);
    let entries_without = list_zip_entries(&bytes_without);
    assert!(entries_with.iter().any(|e| e == ENTRY_IDENTITY));
    assert!(!entries_without.iter().any(|e| e == ENTRY_IDENTITY));
}

#[test]
fn hkdf_subkeys_aes_and_hmac_are_distinct() {
    // Cheap sanity check that proves the namespacing in the public info
    // strings is doing what it should: the same master + different infos
    // produces different subkeys, and neither matches the master.
    let master = [42u8; 32];
    let hkdf = Hkdf::<Sha256>::new(None, &master);
    let mut k_enc = [0u8; 32];
    let mut k_mac = [0u8; 32];
    hkdf.expand(HKDF_INFO_AES, &mut k_enc).unwrap();
    hkdf.expand(HKDF_INFO_HMAC, &mut k_mac).unwrap();
    assert_ne!(k_enc, k_mac);
    assert_ne!(k_enc, master);
    assert_ne!(k_mac, master);

    // Guard against accidental namespace collision with the E2EE namespace.
    assert!(
        std::str::from_utf8(HKDF_INFO_AES)
            .unwrap()
            .starts_with("bibliogenius-backup-v1-")
    );
    assert!(
        std::str::from_utf8(HKDF_INFO_HMAC)
            .unwrap()
            .starts_with("bibliogenius-backup-v1-")
    );
    assert_ne!(HKDF_INFO_AES, HKDF_INFO_HMAC);
}

#[tokio::test]
async fn only_local_covers_are_archived_hub_urls_excluded() {
    let tmp = TempDir::new().unwrap();
    let db = setup_test_db(&tmp).await;
    let cover_dir = tmp.path().join("covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    // Local cover on disk, referenced by a relative filename in cover_url.
    let local_filename = "local-cover.png";
    let local_path = cover_dir.join(local_filename);
    std::fs::write(&local_path, b"fake-png-bytes").unwrap();

    // Local book points at an existing on-disk cover.
    let now = chrono::Utc::now().to_rfc3339();
    book::ActiveModel {
        title: Set("Local Cover Book".into()),
        cover_url: Set(Some(local_filename.to_string())),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Hub-hosted cover: must be skipped.
    book::ActiveModel {
        title: Set("Hub Cover Book".into()),
        cover_url: Set(Some("https://hub.example.org/covers/abc.jpg".into())),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    // Dangling reference: file missing on disk, must be skipped silently.
    book::ActiveModel {
        title: Set("Missing Cover Book".into()),
        cover_url: Set(Some("does-not-exist.png".into())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let out = tmp.path().join("backup.bgbackup");
    write_backup(
        &db,
        &out,
        TEST_SECRET,
        UnlockKind::Passphrase,
        TEST_LIBRARY_UUID,
        None,
        TEST_PREFS,
        &cover_dir,
    )
    .await
    .expect("write_backup");

    let archive = std::fs::read(&out).unwrap();
    let manifest = parse_manifest(&archive);
    assert_eq!(manifest.covers.len(), 1, "exactly one local cover archived");
    assert_eq!(manifest.counts.books, 3);
    let cover = &manifest.covers[0];
    assert_eq!(cover.filename, local_filename);
    // sha256 of "fake-png-bytes"
    let expected = {
        use sha2::Digest;
        let mut h = Sha256::new();
        h.update(b"fake-png-bytes");
        hex::encode(h.finalize())
    };
    assert_eq!(cover.sha256, expected);

    let entries = list_zip_entries(&archive);
    let cover_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.starts_with("covers/"))
        .collect();
    assert_eq!(cover_entries.len(), 1);
}

#[tokio::test]
async fn hmac_signature_validates_intact_archive_and_fails_after_mutation() {
    let tmp = TempDir::new().unwrap();
    let db = setup_test_db(&tmp).await;
    seed_minimal(&db).await;
    let out = tmp.path().join("backup.bgbackup");
    let cover_dir = tmp.path().join("covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    write_backup(
        &db,
        &out,
        TEST_SECRET,
        UnlockKind::Passphrase,
        TEST_LIBRARY_UUID,
        None,
        TEST_PREFS,
        &cover_dir,
    )
    .await
    .expect("write_backup");

    let archive = std::fs::read(&out).unwrap();

    // Re-derive K_mac from the manifest's salt + the same secret.
    let manifest = parse_manifest(&archive);
    let salt_vec = B64.decode(&manifest.argon2.salt_b64).unwrap();
    let salt: [u8; 32] = salt_vec.try_into().unwrap();
    let master = derive_key_from_password(TEST_SECRET, &salt).unwrap();
    let hkdf = Hkdf::<Sha256>::new(None, &master);
    let mut k_mac = [0u8; 32];
    hkdf.expand(HKDF_INFO_HMAC, &mut k_mac).unwrap();

    // Reproduce the documented HMAC input: concatenation of all other
    // entries' raw bytes in order: manifest.json, db.sqlite, prefs.json,
    // identity.bin (if present), covers/* sorted.
    let manifest_bytes = read_zip_entry(&archive, ENTRY_MANIFEST).unwrap();
    let db_bytes = read_zip_entry(&archive, ENTRY_DB).unwrap();
    let prefs_bytes = read_zip_entry(&archive, ENTRY_PREFS).unwrap();
    let stored_signature = read_zip_entry(&archive, ENTRY_SIGNATURE).unwrap();

    // No covers, no identity in this fixture; simple chain.
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&k_mac).unwrap();
    mac.update(&manifest_bytes);
    mac.update(&db_bytes);
    mac.update(&prefs_bytes);
    let computed = mac.finalize().into_bytes();
    assert_eq!(computed.as_slice(), stored_signature.as_slice());

    // Negative path: flip a single byte in the db.sqlite ciphertext and
    // recompute. Must mismatch.
    let mut tampered_db = db_bytes.clone();
    tampered_db[0] ^= 0x01;
    let mut mac_bad = <Hmac<Sha256> as Mac>::new_from_slice(&k_mac).unwrap();
    mac_bad.update(&manifest_bytes);
    mac_bad.update(&tampered_db);
    mac_bad.update(&prefs_bytes);
    let bad = mac_bad.finalize().into_bytes();
    assert_ne!(bad.as_slice(), stored_signature.as_slice());
}

#[tokio::test]
async fn errors_cleanly_when_output_path_is_unwritable() {
    let tmp = TempDir::new().unwrap();
    let db = setup_test_db(&tmp).await;
    let cover_dir = tmp.path().join("covers");
    std::fs::create_dir_all(&cover_dir).unwrap();

    // Force write_atomic / make_tmp_db_path to fail by pointing the output
    // at a path whose parent is a regular file (mkdir -p on a file fails on
    // every supported platform).
    let blocking_file = tmp.path().join("not-a-dir");
    std::fs::write(&blocking_file, b"i am a file, not a directory").unwrap();
    let out: PathBuf = blocking_file.join("inside-a-file.bgbackup");

    let res = write_backup(
        &db,
        &out,
        TEST_SECRET,
        UnlockKind::Passphrase,
        TEST_LIBRARY_UUID,
        None,
        TEST_PREFS,
        &cover_dir,
    )
    .await;

    let err = res.expect_err("must fail when parent is a file");
    assert!(
        matches!(err, BackupError::Io(_) | BackupError::InvalidInput(_)),
        "expected Io or InvalidInput, got: {err:?}"
    );
    // Ensure no archive was left behind.
    assert!(!out.exists());
    // And that the message doesn't accidentally include the secret.
    let msg = format!("{err}");
    assert!(!msg.contains(std::str::from_utf8(TEST_SECRET).unwrap()));
    // No partial files on the temp dir's actual writable area:
    let stale: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("dbtmp") || n.contains("partial"))
        .collect();
    assert!(stale.is_empty(), "unexpected stale files: {stale:?}");

    // backup module reachable for module-level constant audit (suppresses
    // unused import lint when the module path isn't otherwise touched in
    // this test).
    let _: &str = backup::FORMAT_VERSION;
}
