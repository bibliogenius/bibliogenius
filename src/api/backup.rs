//! Local encrypted backup writer (`.bgbackup` format).
//!
//! Implements the producer half of ADR-037. Restoration lives in its own
//! module (PR #3). Cryptographic primitives are reused from
//! `crypto::encryption` per SECURITY_GUIDELINES §B3 (HKDF info-string
//! namespacing) and §B7 (Argon2id parameters).
//!
//! Pipeline:
//!  1. `VACUUM INTO` produces a transactionally-consistent snapshot of the
//!     live SQLite DB without closing the connection.
//!  2. Argon2id derives a 256-bit master from the user secret + a fresh
//!     32-byte salt.
//!  3. HKDF-SHA256 expands the master into two domain-separated subkeys:
//!     `K_enc` (AES-256-GCM) and `K_mac` (HMAC-SHA256).
//!  4. Each entry (`db.sqlite`, `prefs.json`, optional `identity.bin`,
//!     `covers/*`) is encrypted independently with a fresh 12-byte random
//!     nonce prepended to the ciphertext.
//!  5. `manifest.json` is written in clear (counts, hashes, KDF params,
//!     `library_uuid`); it carries no secret data.
//!  6. `signature` (HMAC-SHA256 over the concatenation of every other
//!     entry's raw bytes, in the documented order) gates restore-time
//!     integrity.
//!  7. The archive is built in a `tokio::task::spawn_blocking` worker and
//!     written atomically (write to `*.partial.<uuid>`, fsync, rename).
//!
//! All sensitive intermediate buffers (master key, subkeys, plaintext DB,
//! identity bytes, prefs JSON) are zeroized as soon as they are no longer
//! needed. The caller's `secret` borrow is not modified; the caller is
//! responsible for clearing their own buffer.

use std::collections::HashSet;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::crypto::encryption::{derive_key_from_password, encrypt_aes_gcm, generate_salt};
use crate::infrastructure::db::SCHEMA_VERSION;
use crate::models::{author, book, collection, contact, copy, loan, peer, sale, tag};

/// Format version recorded in the manifest. String (not int) so a future
/// minor revision (`"1.1"`) can add optional fields without breaking v1
/// readers.
pub const FORMAT_VERSION: &str = "1";

pub const ENTRY_MANIFEST: &str = "manifest.json";
pub const ENTRY_DB: &str = "db.sqlite";
pub const ENTRY_PREFS: &str = "prefs.json";
pub const ENTRY_IDENTITY: &str = "identity.bin";
pub const ENTRY_SIGNATURE: &str = "signature";

/// HKDF info string for the AES-GCM subkey. Namespaced under the
/// `bibliogenius-backup-v1-` prefix to prevent collision with the
/// `bibliogenius-e2ee-v1-` family (SECURITY_GUIDELINES §B3).
pub const HKDF_INFO_AES: &[u8] = b"bibliogenius-backup-v1-aes";
/// HKDF info string for the HMAC-SHA256 subkey (same namespace, distinct
/// suffix from `HKDF_INFO_AES`).
pub const HKDF_INFO_HMAC: &[u8] = b"bibliogenius-backup-v1-hmac";

// Argon2id parameters reflected into the manifest. The actual derivation
// runs inside `derive_key_from_password`, which already enforces these
// values (single source of truth).
const ARGON2_M_COST: u32 = 65536;
const ARGON2_T_COST: u32 = 3;
const ARGON2_P_COST: u32 = 4;

/// Identity bytes packed plaintext: ed25519 secret (32B) || x25519 secret (32B).
const IDENTITY_PACKED_LEN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnlockKind {
    RecoveryCode,
    Passphrase,
}

#[derive(Debug)]
pub enum BackupError {
    Io(std::io::Error),
    Db(String),
    Crypto(String),
    Zip(String),
    InvalidInput(String),
    Serialization(String),
    TaskJoin(String),
    /// HMAC verification failed: either the secret is wrong, or the archive
    /// was mutated. Surfaced to the UI verbatim, never includes secret data.
    BadSignature,
    /// `manifest.format_version` is not understood by this build.
    FormatVersionUnknown(String),
    /// Archive's `schema_version` is newer than the running build.
    SchemaTooNew {
        archive: u32,
        current: u32,
    },
    /// Decrypted `db.sqlite` plaintext does not match `manifest.db_sha256`.
    /// Indicates archive corruption or tampering past the HMAC layer.
    DbHashMismatch,
    /// Migration replay on the restored DB failed.
    MigrationFailed(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Db(msg) => write!(f, "database error: {msg}"),
            Self::Crypto(msg) => write!(f, "crypto error: {msg}"),
            Self::Zip(msg) => write!(f, "zip error: {msg}"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::Serialization(msg) => write!(f, "serialization error: {msg}"),
            Self::TaskJoin(msg) => write!(f, "task join error: {msg}"),
            Self::BadSignature => write!(f, "bad signature: wrong secret or tampered archive"),
            Self::FormatVersionUnknown(v) => write!(f, "unknown format_version: {v}"),
            Self::SchemaTooNew { archive, current } => write!(
                f,
                "archive schema_version {archive} is newer than current {current}; update the app first"
            ),
            Self::DbHashMismatch => write!(f, "db_sha256 mismatch after decryption"),
            Self::MigrationFailed(msg) => write!(f, "migration replay failed: {msg}"),
        }
    }
}

impl std::error::Error for BackupError {}

impl From<std::io::Error> for BackupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<sea_orm::DbErr> for BackupError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Db(e.to_string())
    }
}

impl From<crate::crypto::errors::CryptoError> for BackupError {
    fn from(e: crate::crypto::errors::CryptoError) -> Self {
        Self::Crypto(e.to_string())
    }
}

impl From<zip::result::ZipError> for BackupError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e.to_string())
    }
}

impl From<serde_json::Error> for BackupError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Argon2Params {
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt_b64: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CountsSummary {
    pub books: u64,
    pub copies: u64,
    pub loans: u64,
    pub contacts: u64,
    pub authors: u64,
    pub tags: u64,
    pub collections: u64,
    pub peers: u64,
    pub sales: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverEntry {
    /// Original on-disk basename. Recorded for diagnostics only.
    pub filename: String,
    /// SHA-256 of the plaintext cover (hex). Doubles as the entry-name stem.
    pub sha256: String,
}

/// Manifest fields published in clear in `manifest.json`.
///
/// All values listed here are non-sensitive: counts, hashes, dates, the
/// already-public `library_uuid`, and the KDF parameters (the salt is fine
/// to publish; only the secret itself stays private).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub format_version: String,
    pub schema_version: u32,
    pub exported_at: String,
    pub library_uuid: String,
    pub identity_included: bool,
    pub unlock_kind: UnlockKind,
    pub argon2: Argon2Params,
    pub counts: CountsSummary,
    pub db_sha256: String,
    pub covers: Vec<CoverEntry>,
    pub app_version: String,
}

#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub archive_path: PathBuf,
    pub archive_size_bytes: u64,
    pub manifest: ManifestSummary,
}

/// Write a `.bgbackup` archive at `output_path`.
///
/// `secret` is treated as sensitive: an internal copy is zeroized as soon
/// as the master key is computed; the caller remains responsible for
/// clearing their own buffer.
///
/// `identity_secret_bytes` is `Some((ed25519_secret, x25519_secret))` to
/// include the node identity (Option C clone mode), `None` to skip the
/// `identity.bin` entry entirely. No empty blob is ever written.
///
/// `cover_dir` is the directory where local covers live. Files referenced
/// by `books.cover_url` whose value resolves to a path inside `cover_dir`
/// are included; hub-hosted URLs and dangling references are skipped
/// silently.
#[allow(clippy::too_many_arguments)]
pub async fn write_backup(
    db: &DatabaseConnection,
    output_path: &Path,
    secret: &[u8],
    unlock_kind: UnlockKind,
    library_uuid: &str,
    identity_secret_bytes: Option<([u8; 32], [u8; 32])>,
    prefs_json: &str,
    cover_dir: &Path,
) -> Result<BackupSummary, BackupError> {
    if secret.is_empty() {
        return Err(BackupError::InvalidInput("empty secret".into()));
    }
    if library_uuid.is_empty() {
        return Err(BackupError::InvalidInput("empty library_uuid".into()));
    }

    // Resolve the temp DB path early so a bad output destination fails
    // fast, before the ~0.5s Argon2 derivation runs.
    let tmp_db_path = make_tmp_db_path(output_path)?;
    let _cleanup = TempFileGuard::new(tmp_db_path.clone());

    snapshot_db(db, &tmp_db_path).await?;
    let counts = compute_counts(db).await?;
    let cover_inputs = collect_local_cover_inputs(db, cover_dir).await?;

    // Owned copies for the blocking worker (Send + 'static).
    let secret_owned = secret.to_vec();
    let prefs_owned = prefs_json.to_string();
    let library_uuid_owned = library_uuid.to_string();
    let output_owned = output_path.to_path_buf();
    let tmp_db_for_thread = tmp_db_path.clone();

    let summary = tokio::task::spawn_blocking(move || -> Result<BackupSummary, BackupError> {
        write_backup_blocking(
            secret_owned,
            unlock_kind,
            library_uuid_owned,
            identity_secret_bytes,
            prefs_owned,
            cover_inputs,
            counts,
            tmp_db_for_thread,
            output_owned,
        )
    })
    .await
    .map_err(|e| BackupError::TaskJoin(e.to_string()))??;

    tracing::info!(
        path = %summary.archive_path.display(),
        bytes = summary.archive_size_bytes,
        books = summary.manifest.counts.books,
        identity_included = summary.manifest.identity_included,
        "wrote .bgbackup archive"
    );

    Ok(summary)
}

// -----------------------------------------------------------------------------
// internals
// -----------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn write_backup_blocking(
    mut secret: Vec<u8>,
    unlock_kind: UnlockKind,
    library_uuid: String,
    identity_bytes: Option<([u8; 32], [u8; 32])>,
    prefs_json: String,
    cover_inputs: Vec<CoverInput>,
    counts: CountsSummary,
    tmp_db_path: PathBuf,
    output_path: PathBuf,
) -> Result<BackupSummary, BackupError> {
    // 1. Argon2id master + HKDF subkeys.
    let salt = generate_salt();
    let mut master = derive_key_from_password(&secret, &salt)?;
    secret.zeroize();

    let (mut k_enc, mut k_mac) = derive_subkeys(&master)?;
    master.zeroize();

    // 2. Snapshot DB plaintext digest (recorded in the manifest in clear).
    let mut db_plaintext = std::fs::read(&tmp_db_path)?;
    let db_sha = sha256_hex(&db_plaintext);

    // 3. Read and encrypt covers, deduping by content hash.
    let mut enc_covers: Vec<(String, Vec<u8>)> = Vec::with_capacity(cover_inputs.len());
    let mut covers_meta: Vec<CoverEntry> = Vec::with_capacity(cover_inputs.len());
    let mut seen_hashes: HashSet<String> = HashSet::new();
    for inp in &cover_inputs {
        let mut bytes = std::fs::read(&inp.path)?;
        let h = sha256_hex(&bytes);
        if !seen_hashes.insert(h.clone()) {
            bytes.zeroize();
            continue;
        }
        let ext = inp
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_lowercase();
        let zip_name = format!("covers/{h}.{ext}");
        covers_meta.push(CoverEntry {
            filename: inp.filename.clone(),
            sha256: h.clone(),
        });
        let ct = seal_entry(&k_enc, &bytes)?;
        bytes.zeroize();
        enc_covers.push((zip_name, ct));
    }
    enc_covers.sort_by(|a, b| a.0.cmp(&b.0));
    covers_meta.sort_by(|a, b| a.sha256.cmp(&b.sha256));

    // 4. Manifest in clear.
    let manifest = ManifestSummary {
        format_version: FORMAT_VERSION.to_string(),
        schema_version: SCHEMA_VERSION,
        exported_at: Utc::now().to_rfc3339(),
        library_uuid,
        identity_included: identity_bytes.is_some(),
        unlock_kind,
        argon2: Argon2Params {
            m_cost: ARGON2_M_COST,
            t_cost: ARGON2_T_COST,
            p_cost: ARGON2_P_COST,
            salt_b64: B64.encode(salt),
        },
        counts,
        db_sha256: db_sha,
        covers: covers_meta,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;

    // 5. Encrypt remaining entries.
    let enc_db = seal_entry(&k_enc, &db_plaintext)?;
    db_plaintext.zeroize();
    let mut prefs_bytes = prefs_json.into_bytes();
    let enc_prefs = seal_entry(&k_enc, &prefs_bytes)?;
    prefs_bytes.zeroize();

    let enc_identity = match identity_bytes {
        Some((mut ed, mut x)) => {
            let mut packed = [0u8; IDENTITY_PACKED_LEN];
            packed[..32].copy_from_slice(&ed);
            packed[32..].copy_from_slice(&x);
            ed.zeroize();
            x.zeroize();
            let sealed = seal_entry(&k_enc, &packed)?;
            packed.zeroize();
            Some(sealed)
        }
        None => None,
    };

    // 6. HMAC-SHA256 over the documented entry order.
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&k_mac)
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    mac.update(&manifest_bytes);
    mac.update(&enc_db);
    mac.update(&enc_prefs);
    if let Some(ref ei) = enc_identity {
        mac.update(ei);
    }
    for (_, ct) in &enc_covers {
        mac.update(ct);
    }
    let signature = mac.finalize().into_bytes().to_vec();

    // Subkeys done; wipe.
    k_enc.zeroize();
    k_mac.zeroize();

    // 7. Build ZIP, then atomic-write to disk.
    let archive_bytes = build_zip(
        &manifest_bytes,
        &enc_db,
        &enc_prefs,
        enc_identity.as_deref(),
        &enc_covers,
        &signature,
    )?;
    write_atomic(&output_path, &archive_bytes)?;

    let archive_size_bytes = std::fs::metadata(&output_path)?.len();
    Ok(BackupSummary {
        archive_path: output_path,
        archive_size_bytes,
        manifest,
    })
}

fn derive_subkeys(master: &[u8; 32]) -> Result<([u8; 32], [u8; 32]), BackupError> {
    let hkdf = Hkdf::<Sha256>::new(None, master);
    let mut enc = [0u8; 32];
    hkdf.expand(HKDF_INFO_AES, &mut enc)
        .map_err(|_| BackupError::Crypto("hkdf expand aes".into()))?;
    let mut mac = [0u8; 32];
    hkdf.expand(HKDF_INFO_HMAC, &mut mac)
        .map_err(|_| BackupError::Crypto("hkdf expand hmac".into()))?;
    Ok((enc, mac))
}

fn seal_entry(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, BackupError> {
    let (nonce, ct) = encrypt_aes_gcm(key, plaintext)?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

async fn snapshot_db(db: &DatabaseConnection, target: &Path) -> Result<(), BackupError> {
    let target_str = target
        .to_str()
        .ok_or_else(|| BackupError::InvalidInput("non-utf8 temp path".into()))?;
    // VACUUM INTO does not accept bind parameters and we control the path
    // (deterministic placement next to `output_path`). Defense-in-depth:
    // refuse anything containing a single quote, which would close the
    // SQL string literal.
    if target_str.contains('\'') {
        return Err(BackupError::InvalidInput("invalid temp path".into()));
    }
    // VACUUM INTO cannot run inside a transaction and is not preparable;
    // `execute_unprepared` sends the raw statement on a fresh statement
    // handle, which is the supported path for VACUUM in SQLite.
    let sql = format!("VACUUM INTO '{target_str}'");
    db.execute_unprepared(&sql).await?;
    if !target.is_file() {
        // SQLite returned success but the file is missing. This happens on
        // some `sqlite::memory:` setups where the driver silently swallows
        // the INTO clause. Production callers always pass a file-backed
        // connection, so this is treated as a hard error here too.
        return Err(BackupError::Db(
            "VACUUM INTO produced no file (unsupported on this connection)".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct CoverInput {
    /// Resolved absolute path of the cover file on disk.
    path: PathBuf,
    /// Original basename, recorded in the manifest for diagnostics.
    filename: String,
}

async fn collect_local_cover_inputs(
    db: &DatabaseConnection,
    cover_dir: &Path,
) -> Result<Vec<CoverInput>, BackupError> {
    let canonical_dir = match cover_dir.canonicalize() {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };
    let books_with_cover = book::Entity::find()
        .filter(book::Column::CoverUrl.is_not_null())
        .all(db)
        .await?;
    let mut out = Vec::new();
    let mut already_seen: HashSet<PathBuf> = HashSet::new();
    for b in books_with_cover {
        let Some(url) = b.cover_url else { continue };
        if url.starts_with("http://") || url.starts_with("https://") {
            continue;
        }
        let candidate = if Path::new(&url).is_absolute() {
            PathBuf::from(&url)
        } else {
            canonical_dir.join(&url)
        };
        let resolved = match candidate.canonicalize() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Path-traversal guard: the resolved file MUST live inside
        // cover_dir. Anything outside is silently dropped.
        if !resolved.starts_with(&canonical_dir) {
            continue;
        }
        if !resolved.is_file() {
            continue;
        }
        if !already_seen.insert(resolved.clone()) {
            continue;
        }
        let filename = resolved
            .file_name()
            .and_then(|f| f.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        out.push(CoverInput {
            path: resolved,
            filename,
        });
    }
    Ok(out)
}

async fn compute_counts(db: &DatabaseConnection) -> Result<CountsSummary, BackupError> {
    Ok(CountsSummary {
        books: book::Entity::find().count(db).await?,
        copies: copy::Entity::find().count(db).await?,
        loans: loan::Entity::find().count(db).await?,
        contacts: contact::Entity::find().count(db).await?,
        authors: author::Entity::find().count(db).await?,
        tags: tag::Entity::find().count(db).await?,
        collections: collection::Entity::find().count(db).await?,
        peers: peer::Entity::find().count(db).await?,
        sales: sale::Entity::find().count(db).await?,
    })
}

fn build_zip(
    manifest_bytes: &[u8],
    enc_db: &[u8],
    enc_prefs: &[u8],
    enc_identity: Option<&[u8]>,
    enc_covers: &[(String, Vec<u8>)],
    signature: &[u8],
) -> Result<Vec<u8>, BackupError> {
    let initial_cap = manifest_bytes.len() + enc_db.len() + enc_prefs.len() + 4096;
    let buf = Cursor::new(Vec::with_capacity(initial_cap));
    let mut zw = zip::ZipWriter::new(buf);

    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflate = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zw.start_file(ENTRY_MANIFEST, stored)?;
    zw.write_all(manifest_bytes)?;
    zw.start_file(ENTRY_DB, deflate)?;
    zw.write_all(enc_db)?;
    zw.start_file(ENTRY_PREFS, deflate)?;
    zw.write_all(enc_prefs)?;
    if let Some(ei) = enc_identity {
        zw.start_file(ENTRY_IDENTITY, deflate)?;
        zw.write_all(ei)?;
    }
    for (name, ct) in enc_covers {
        zw.start_file(name, deflate)?;
        zw.write_all(ct)?;
    }
    zw.start_file(ENTRY_SIGNATURE, stored)?;
    zw.write_all(signature)?;

    Ok(zw.finish()?.into_inner())
}

fn make_tmp_db_path(output_path: &Path) -> Result<PathBuf, BackupError> {
    let parent = output_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let base = output_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("backup");
    let name = format!("{base}.dbtmp.{}", uuid::Uuid::new_v4().simple());
    Ok(parent.join(name))
}

fn write_atomic(target: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let base = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("backup");
    let tmp = parent.join(format!("{base}.partial.{}", uuid::Uuid::new_v4().simple()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// =============================================================================
// Reader (.bgbackup) -- ADR-037 §5
//
// Symmetric to the writer above. The pipeline guarantees that, at every step,
// either the live DB at `db_path` is intact, or a `*.rollback-<ts>` sibling is
// available for `restore_from_rollback`. There is no window where neither is
// usable.
//
// In-process semantics: this module never touches the FFI's global SeaORM
// connection. It opens its own ephemeral connections on file paths and closes
// them before returning. The Flutter wizard is expected to force-restart the
// app after a successful Replace so the new on-disk DB is picked up cleanly
// (ADR-037 §5 implementation note "Recommandation forte : redémarrage forcé").
// =============================================================================

/// Restoration mode chosen by the user.
///
/// `Replace` performs a full atomic swap of the live DB and rebuilds the cover
/// directory. `Merge` upserts the archive's catalog rows on top of the live DB,
/// leaving identity, peers, oplog, notifications, and any install-specific
/// table untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    Replace,
    Merge,
}

/// Result returned to the Flutter wizard after a successful restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSummary {
    pub mode: RestoreMode,
    /// True iff the archive carried `identity.bin` and the user opted in to
    /// clone-mode (Replace + `restore_identity == true`).
    pub identity_restored: bool,
    /// True iff the caller passed a `local_library_uuid` matching the
    /// archive's manifest UUID -- i.e. the user is restoring a backup
    /// produced by THIS device. The Replace path then preserves the live
    /// `crypto_keys` row instead of wiping it (same device, same identity,
    /// same encryption key); the caller MUST NOT touch local storage.
    /// Always `false` for Merge.
    pub same_device: bool,
    /// `Some(uuid)` when the caller MUST persist this `library_uuid` to local
    /// storage (the Replace + clone-mode path). `None` after Replace without
    /// clone AND when the caller is on a different device means the caller
    /// MUST clear the local `library_uuid` so the next launch generates a
    /// fresh one (clean-install path). `None` after Merge or Replace
    /// same-device means the caller MUST NOT touch the existing
    /// `library_uuid`.
    pub restored_library_uuid: Option<String>,
    /// JSON-encoded prefs payload from the archive. Caller applies a
    /// whitelist (PR #4 will move this whitelist into Rust). Empty string in
    /// Merge mode (prefs are intentionally ignored on Merge per ticket).
    pub prefs_json: String,
    /// Path of the rollback file created by Replace, surfaced for UI hints.
    /// `None` for Merge.
    pub rollback_path: Option<String>,
    pub books_after: i64,
    pub copies_after: i64,
    pub contacts_after: i64,
    pub covers_restored: i64,
}

/// Metadata about an existing rollback file shown in the "Restore previous
/// version" UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackInfo {
    pub path: String,
    pub created_at: String,
    pub age_seconds: i64,
    pub size_bytes: i64,
}

/// Maximum age (seconds) of a rollback or replaced sibling kept on disk. After
/// this, `run_startup_maintenance` purges it. 24h per ADR-037 §5.
pub const ROLLBACK_TTL_SECONDS: i64 = 24 * 3600;

/// Filename suffix used by `restore_backup` Replace to preserve the previous
/// live DB.
pub const ROLLBACK_SUFFIX: &str = ".rollback-";
/// Filename suffix used by `restore_from_rollback` to preserve the DB it just
/// replaced.
pub const REPLACED_SUFFIX: &str = ".replaced-";

// -----------------------------------------------------------------------------
// Public reader API
// -----------------------------------------------------------------------------

/// Parse `manifest.json` from an archive without unlocking. Used by the wizard
/// to display the preview screen before prompting for the secret.
pub fn read_manifest(archive_path: &Path) -> Result<ManifestSummary, BackupError> {
    let f = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(f)?;
    let bytes = read_zip_entry_bytes(&mut zip, ENTRY_MANIFEST)?;
    let manifest: ManifestSummary = serde_json::from_slice(&bytes)?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(BackupError::FormatVersionUnknown(manifest.format_version));
    }
    Ok(manifest)
}

/// Verify the archive HMAC against `secret`. Returns `Ok(())` for an intact
/// archive + correct secret; `Err(BackupError::BadSignature)` for either a
/// wrong secret OR a single-byte mutation in any signed entry.
///
/// Runs Argon2id internally (~0.5-1s); should be invoked off the UI thread.
pub fn verify_signature(archive_path: &Path, secret: &[u8]) -> Result<(), BackupError> {
    if secret.is_empty() {
        return Err(BackupError::InvalidInput("empty secret".into()));
    }
    let f = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(f)?;
    let manifest = parse_manifest_from_zip(&mut zip)?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(BackupError::FormatVersionUnknown(manifest.format_version));
    }
    let (mut k_enc, mut k_mac) = derive_keys_from_secret(secret, &manifest)?;
    let result = check_signature(&mut zip, &k_mac, &manifest);
    k_enc.zeroize();
    k_mac.zeroize();
    result
}

/// Restore an archive into `db_path`. Pipeline branches on `mode`; both modes
/// run pre-checks (manifest parse, HMAC, schema version) before touching disk
/// state, so a failed unlock or a stale archive never leaves the live DB
/// inconsistent.
///
/// `secret` is treated as sensitive: an internal copy is zeroized as soon as
/// the master key is derived; the caller remains responsible for clearing the
/// outer buffer.
///
/// `restore_identity` is honoured only in Replace mode and only when
/// `identity.bin` is present in the archive; it is silently ignored otherwise.
///
/// `local_library_uuid` lets the caller signal "this device's current
/// `library_uuid`". When the archive's manifest UUID matches, the Replace
/// path preserves the existing `crypto_keys` row -- typical auto-backup
/// case where the archive was produced by THIS device with the same
/// identity (ADR-037 §5 same-device path). `None` falls back to the
/// pre-existing behaviour (wipe `crypto_keys` unless full clone-mode).
///
/// On Replace success, the previous live DB is preserved at
/// `<db_path>.rollback-<ts>` and surfaced in `RestoreSummary.rollback_path`.
/// `run_startup_maintenance` purges this file 24h later.
pub async fn restore_backup(
    archive_path: &Path,
    secret: &[u8],
    mode: RestoreMode,
    restore_identity: bool,
    local_library_uuid: Option<String>,
    db_path: &Path,
    cover_dir: &Path,
) -> Result<RestoreSummary, BackupError> {
    if secret.is_empty() {
        return Err(BackupError::InvalidInput("empty secret".into()));
    }
    if !archive_path.is_file() {
        return Err(BackupError::InvalidInput(format!(
            "archive not found: {}",
            archive_path.display()
        )));
    }
    if !db_path.is_file() {
        return Err(BackupError::InvalidInput(format!(
            "live db not found: {}",
            db_path.display()
        )));
    }

    // The pipeline mixes sync work (Argon2, AES-GCM, zip, file renames)
    // with async work (SeaORM migrations and crypto_keys ops). We run it
    // inline on the current runtime rather than splitting between
    // `spawn_blocking` and the executor: Argon2 dominates (~500-1000ms)
    // and would have to be re-coordinated across the boundary anyway.
    // Callers always invoke this from a foreground UI flow with a spinner.
    let mut secret_z = zeroize::Zeroizing::new(secret.to_vec());
    let summary = restore_backup_inner(
        archive_path,
        &secret_z,
        mode,
        restore_identity,
        local_library_uuid,
        db_path,
        cover_dir,
    )
    .await;
    secret_z.zeroize();
    let summary = summary?;

    tracing::info!(
        mode = ?summary.mode,
        identity_restored = summary.identity_restored,
        books_after = summary.books_after,
        covers_restored = summary.covers_restored,
        rollback_path = ?summary.rollback_path,
        "restore complete"
    );
    Ok(summary)
}

/// Scan the directory of `db_path` for rollback siblings and return them as a
/// list ordered most-recent first.
pub fn list_available_rollbacks(db_path: &Path) -> Vec<RollbackInfo> {
    let parent = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    let base = db_path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if base.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{base}{ROLLBACK_SUFFIX}");
    let now = chrono::Utc::now();
    let read = match std::fs::read_dir(&parent) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<RollbackInfo> = read
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with(&prefix))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_str()?;
            let suffix = name.strip_prefix(&prefix)?;
            let ts = parse_rollback_timestamp(suffix)?;
            let age_seconds = (now - ts).num_seconds().max(0);
            let size_bytes = std::fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0) as i64;
            Some(RollbackInfo {
                path: path.to_string_lossy().into_owned(),
                created_at: ts.to_rfc3339(),
                age_seconds,
                size_bytes,
            })
        })
        .collect();
    out.sort_by_key(|info| info.age_seconds);
    out
}

/// Swap a rollback file back into the live DB position. The DB currently at
/// `db_path` is preserved at `<db_path>.replaced-<ts>` so the user can chain
/// reverts within 24h.
///
/// **The caller MUST force-restart the app** after this call returns, for the
/// same reasons as `restore_backup` Replace: the FFI connection's open file
/// descriptors still point to the now-displaced inode.
pub async fn restore_from_rollback(
    rollback_path: &Path,
    db_path: &Path,
) -> Result<(), BackupError> {
    if !rollback_path.is_file() {
        return Err(BackupError::InvalidInput(format!(
            "rollback file not found: {}",
            rollback_path.display()
        )));
    }
    if !db_path.is_file() {
        return Err(BackupError::InvalidInput(format!(
            "live db not found: {}",
            db_path.display()
        )));
    }
    let rollback_owned = rollback_path.to_path_buf();
    let db_owned = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), BackupError> {
        let ts = chrono::Utc::now();
        let replaced = sibling_with_suffix(&db_owned, REPLACED_SUFFIX, &ts);
        // 1. Live DB -> replaced sibling (atomic).
        rename_db_with_wal(&db_owned, &replaced)?;
        // 2. Rollback -> live (atomic).
        rename_db_with_wal(&rollback_owned, &db_owned)?;
        tracing::info!(
            rolled_back_from = %rollback_owned.display(),
            previous_now_at = %replaced.display(),
            "rollback restored"
        );
        Ok(())
    })
    .await
    .map_err(|e| BackupError::TaskJoin(e.to_string()))??;
    Ok(())
}

// -----------------------------------------------------------------------------
// Replace + Merge pipelines (private)
// -----------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn restore_backup_inner(
    archive_path: &Path,
    secret: &[u8],
    mode: RestoreMode,
    restore_identity: bool,
    local_library_uuid: Option<String>,
    db_path: &Path,
    cover_dir: &Path,
) -> Result<RestoreSummary, BackupError> {
    // 1. Pre-checks (manifest, format, schema, HMAC). No disk side-effects yet.
    let f = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(f)?;
    let manifest = parse_manifest_from_zip(&mut zip)?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(BackupError::FormatVersionUnknown(manifest.format_version));
    }
    if manifest.schema_version > SCHEMA_VERSION {
        return Err(BackupError::SchemaTooNew {
            archive: manifest.schema_version,
            current: SCHEMA_VERSION,
        });
    }
    let (mut k_enc, mut k_mac) = derive_keys_from_secret(secret, &manifest)?;
    if let Err(e) = check_signature(&mut zip, &k_mac, &manifest) {
        k_enc.zeroize();
        k_mac.zeroize();
        return Err(e);
    }
    k_mac.zeroize(); // No longer needed past this point.

    // 2. Decrypt db.sqlite into a temp file inside the same dir as db_path so
    //    the eventual rename is atomic (single FS).
    let ts = chrono::Utc::now();
    let tmp_db_path = sibling_with_suffix(db_path, ".restore-", &ts);
    let _tmp_guard = TempFileGuard::new(tmp_db_path.clone());

    let enc_db = read_zip_entry_bytes(&mut zip, ENTRY_DB)?;
    let mut db_plaintext = unseal_entry(&k_enc, &enc_db)?;
    drop(enc_db);

    // 3. Verify db_sha256 against the manifest BEFORE writing anything.
    let computed_hash = sha256_hex(&db_plaintext);
    if !ct_eq_str(&computed_hash, &manifest.db_sha256) {
        db_plaintext.zeroize();
        k_enc.zeroize();
        return Err(BackupError::DbHashMismatch);
    }
    std::fs::write(&tmp_db_path, &db_plaintext)?;
    db_plaintext.zeroize();

    // 4. Build the cover plaintext map (sha256 -> bytes), needed by both modes
    //    to (re)write covers from the archive.
    let cover_plaintext = collect_cover_plaintext(&mut zip, &k_enc, &manifest)?;

    // 5. Decrypt prefs.json + identity.bin if present.
    let enc_prefs = read_zip_entry_bytes(&mut zip, ENTRY_PREFS)?;
    let mut prefs_bytes = unseal_entry(&k_enc, &enc_prefs)?;
    let prefs_json = String::from_utf8_lossy(&prefs_bytes).into_owned();
    prefs_bytes.zeroize();
    drop(enc_prefs);

    let identity_packed: Option<zeroize::Zeroizing<Vec<u8>>> = if manifest.identity_included {
        let enc_id = read_zip_entry_bytes(&mut zip, ENTRY_IDENTITY)?;
        let plain = unseal_entry(&k_enc, &enc_id)?;
        if plain.len() != IDENTITY_PACKED_LEN {
            k_enc.zeroize();
            return Err(BackupError::Crypto("identity payload length".into()));
        }
        Some(zeroize::Zeroizing::new(plain))
    } else {
        None
    };
    drop(zip);
    k_enc.zeroize();

    // 6. Mode-specific writes.
    let summary = match mode {
        RestoreMode::Replace => {
            apply_replace(
                db_path,
                cover_dir,
                &tmp_db_path,
                &manifest,
                &cover_plaintext,
                prefs_json.clone(),
                identity_packed.as_ref().map(|z| z.as_slice()),
                restore_identity,
                local_library_uuid.as_deref(),
                &ts,
            )
            .await?
        }
        RestoreMode::Merge => {
            apply_merge(
                db_path,
                cover_dir,
                &tmp_db_path,
                &manifest,
                &cover_plaintext,
            )
            .await?
        }
    };

    // 7. tmp_db_path is removed by `_tmp_guard` on drop.
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
async fn apply_replace(
    db_path: &Path,
    cover_dir: &Path,
    tmp_db_path: &Path,
    manifest: &ManifestSummary,
    cover_plaintext: &std::collections::HashMap<String, (Vec<u8>, String)>,
    prefs_json: String,
    identity_packed: Option<&[u8]>,
    restore_identity_opt: bool,
    local_library_uuid: Option<&str>,
    ts: &chrono::DateTime<chrono::Utc>,
) -> Result<RestoreSummary, BackupError> {
    // 1. Migrate the tmp DB forward to the running schema_version. We do this
    //    BEFORE the swap so a migration failure leaves the live DB untouched.
    if manifest.schema_version < SCHEMA_VERSION {
        run_migrations_on_path(tmp_db_path)
            .await
            .map_err(|e| BackupError::MigrationFailed(e.to_string()))?;
    }

    // 2. Swap: live -> rollback, tmp -> live. Both renames are atomic on the
    //    same filesystem (we placed tmp next to live for that reason).
    let rollback_path = sibling_with_suffix(db_path, ROLLBACK_SUFFIX, ts);
    rename_db_with_wal(db_path, &rollback_path)?;
    if let Err(e) = rename_db_with_wal(tmp_db_path, db_path) {
        // Roll back the first rename: live is still recoverable.
        let _ = rename_db_with_wal(&rollback_path, db_path);
        return Err(e.into());
    }

    // 3. Identity handling on the freshly restored live DB.
    //
    // Three cases:
    //   - Replace + clone-mode: re-encrypt archive's identity bytes with
    //     manifest UUID. Caller persists the manifest UUID locally.
    //   - Replace + same-device (typical auto-backup restore): the archive's
    //     `crypto_keys` row was encrypted with the same `library_uuid` the
    //     device still carries, so the live `NodeIdentity` decrypts it
    //     unchanged. Skip both `rewrite_crypto_keys` and `clear_crypto_keys`.
    //     Caller does not touch local storage. Preserves peer relationships
    //     across catalog rollback (ADR-037 §5).
    //   - Replace + cross-device + no clone: wipe the row so the next launch
    //     generates a fresh NodeIdentity (clean-install path).
    // `local_library_uuid` is the device's CURRENT persisted uuid, read by the
    // caller WITHOUT minting one on a miss (see `auth.peekLibraryUuid` in
    // Flutter). An absent or blank value therefore means the device's identity
    // is genuinely unknown here, NOT that it differs from the archive: we must
    // never let an unknown local uuid masquerade as a same-device match, nor
    // let a caller that minted a junk uuid mid-restore silently flip the branch
    // (ADR-042 §13.3). Blank is normalized to "absent".
    let same_device = match local_library_uuid {
        Some(local) if !local.trim().is_empty() => local == manifest.library_uuid.as_str(),
        _ => false,
    };
    let identity_restored = match (restore_identity_opt, identity_packed) {
        (true, Some(packed)) => {
            let ed_bytes: [u8; 32] = packed[..32]
                .try_into()
                .map_err(|_| BackupError::Crypto("identity ed slice".into()))?;
            let x_bytes: [u8; 32] = packed[32..]
                .try_into()
                .map_err(|_| BackupError::Crypto("identity x slice".into()))?;
            rewrite_crypto_keys(db_path, &manifest.library_uuid, &ed_bytes, &x_bytes).await?;
            true
        }
        _ if same_device => {
            // No-op: the row in the restored DB was written by THIS device's
            // previous self with the same `library_uuid`. The local
            // NodeIdentity is intact and decrypts the row as-is.
            false
        }
        _ => {
            clear_crypto_keys(db_path).await?;
            false
        }
    };

    // 4. Rebuild the cover directory: wipe everything inside, then write the
    //    archive's covers verbatim. Hub-hosted covers (https URLs) live in
    //    the books table, not in cover_dir, so they are unaffected.
    let _ = std::fs::create_dir_all(cover_dir);
    wipe_cover_dir(cover_dir)?;
    let covers_restored = write_covers(cover_dir, cover_plaintext)?;

    // 5. Read counts from the restored DB.
    let counts = count_after(db_path).await?;

    let restored_library_uuid = if identity_restored {
        Some(manifest.library_uuid.clone())
    } else {
        None
    };

    Ok(RestoreSummary {
        mode: RestoreMode::Replace,
        identity_restored,
        same_device,
        restored_library_uuid,
        prefs_json,
        rollback_path: Some(rollback_path.to_string_lossy().into_owned()),
        books_after: counts.0,
        copies_after: counts.1,
        contacts_after: counts.2,
        covers_restored,
    })
}

async fn apply_merge(
    db_path: &Path,
    cover_dir: &Path,
    tmp_db_path: &Path,
    _manifest: &ManifestSummary,
    cover_plaintext: &std::collections::HashMap<String, (Vec<u8>, String)>,
) -> Result<RestoreSummary, BackupError> {
    // Open a read-only connection on the tmp DB to load the merge whitelist.
    let import_payload = load_merge_payload(tmp_db_path).await?;

    // Open a writable connection on the live DB and run the existing upsert.
    // Multiple connections on the same SQLite file are supported (WAL mode);
    // the FFI's live connection sees the new rows on its next read.
    apply_upsert(db_path, import_payload).await?;

    // Additive cover restore: write entries that don't already exist on disk.
    let _ = std::fs::create_dir_all(cover_dir);
    let covers_restored = write_covers_additive(cover_dir, cover_plaintext)?;

    let counts = count_after(db_path).await?;

    Ok(RestoreSummary {
        mode: RestoreMode::Merge,
        identity_restored: false,
        same_device: false,
        restored_library_uuid: None,
        prefs_json: String::new(),
        rollback_path: None,
        books_after: counts.0,
        copies_after: counts.1,
        contacts_after: counts.2,
        covers_restored,
    })
}

// -----------------------------------------------------------------------------
// DB helpers (open ephemeral connection on a path)
// -----------------------------------------------------------------------------

/// Migrate the DB at `path` forward to `SCHEMA_VERSION`. Idempotent: each
/// individual migration in `run_migrations` is guarded by `IF NOT EXISTS`
/// or equivalent.
async fn run_migrations_on_path(path: &Path) -> Result<(), sea_orm::DbErr> {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let db = sea_orm::Database::connect(&url).await?;
    crate::infrastructure::db::run_migrations(&db).await?;
    db.close().await?;
    Ok(())
}

async fn rewrite_crypto_keys(
    db_path: &Path,
    library_uuid: &str,
    ed_bytes: &[u8; 32],
    x_bytes: &[u8; 32],
) -> Result<(), BackupError> {
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let db = sea_orm::Database::connect(&url)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    db.execute_unprepared("DELETE FROM crypto_keys WHERE user_id = 0")
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    crate::services::identity_service::store_identity_bytes(&db, library_uuid, ed_bytes, x_bytes)
        .await
        .map_err(BackupError::Db)?;
    db.close()
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    Ok(())
}

async fn clear_crypto_keys(db_path: &Path) -> Result<(), BackupError> {
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let db = sea_orm::Database::connect(&url)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    db.execute_unprepared("DELETE FROM crypto_keys WHERE user_id = 0")
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    db.close()
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    Ok(())
}

async fn count_after(db_path: &Path) -> Result<(i64, i64, i64), BackupError> {
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let db = sea_orm::Database::connect(&url)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    let books = book::Entity::find()
        .count(&db)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))? as i64;
    let copies = copy::Entity::find()
        .count(&db)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))? as i64;
    let contacts = contact::Entity::find()
        .count(&db)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))? as i64;
    db.close()
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    Ok((books, copies, contacts))
}

async fn load_merge_payload(
    tmp_db_path: &Path,
) -> Result<crate::api::export::ImportBackupData, BackupError> {
    use crate::api::export::ImportBackupData;
    use crate::models::{
        author, book, book_authors, book_tags, collection, collection_book, contact, copy,
        gamification_achievements, gamification_config, gamification_progress,
        gamification_streaks, library_config, loan, sale, tag,
    };

    let url = format!("sqlite://{}?mode=ro", tmp_db_path.display());
    let db = sea_orm::Database::connect(&url)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;

    // Models in the merge whitelist (per ticket §"Pipeline Merge"). Not loaded:
    // peers, crypto_keys, peer_book, operation_log, notification, relay_config,
    // linked_device, installation_profile, user.
    let library_config = library_config::Entity::find()
        .one(&db)
        .await
        .unwrap_or(None);
    let books_models = book::Entity::find().all(&db).await.unwrap_or_default();
    let authors = author::Entity::find().all(&db).await.unwrap_or_default();
    let book_authors = book_authors::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let copies = copy::Entity::find().all(&db).await.unwrap_or_default();
    let contacts_models = contact::Entity::find().all(&db).await.unwrap_or_default();
    let loans = loan::Entity::find().all(&db).await.unwrap_or_default();
    let sales = sale::Entity::find().all(&db).await.unwrap_or_default();
    let tags_models = tag::Entity::find().all(&db).await.unwrap_or_default();
    let book_tags = book_tags::Entity::find().all(&db).await.unwrap_or_default();
    let collections = collection::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let collection_books = collection_book::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let gamification_config_row = gamification_config::Entity::find()
        .one(&db)
        .await
        .unwrap_or(None);
    let gamification_progress = gamification_progress::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let gamification_achievements = gamification_achievements::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    let gamification_streaks = gamification_streaks::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();
    db.close()
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;

    let books = books_models
        .into_iter()
        .map(book_model_to_import_book)
        .collect();
    let contacts = contacts_models
        .into_iter()
        .map(contact_model_to_import_contact)
        .collect();
    let tags = tags_models
        .into_iter()
        .map(tag_model_to_import_tag)
        .collect();

    Ok(ImportBackupData {
        version: Some(FORMAT_VERSION.to_string()),
        exported_at: None,
        library_config,
        books: Some(books),
        authors: Some(authors),
        book_authors: Some(book_authors),
        copies: Some(copies),
        contacts: Some(contacts),
        loans: Some(loans),
        sales: Some(sales),
        tags: Some(tags),
        book_tags: Some(book_tags),
        collections: Some(collections),
        collection_books: Some(collection_books),
        // Peers excluded by the merge whitelist; sending None makes the
        // existing skip in `run_import_upsert` step 12 trivially apply.
        peers: None,
        gamification_config: gamification_config_row,
        gamification_progress: Some(gamification_progress),
        gamification_achievements: Some(gamification_achievements),
        gamification_streaks: Some(gamification_streaks),
    })
}

async fn apply_upsert(
    db_path: &Path,
    payload: crate::api::export::ImportBackupData,
) -> Result<(), BackupError> {
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    let db = sea_orm::Database::connect(&url)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    let _ = crate::api::export::run_import_upsert(&db, payload).await;
    db.close()
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    Ok(())
}

// Conversions Model -> Import* mirroring the JSON roundtrip we'd do via
// serde. Keeps the merge path purely in-process (no JSON intermediate).

fn book_model_to_import_book(m: crate::models::book::Model) -> crate::api::export::ImportBook {
    crate::api::export::ImportBook {
        id: Some(m.id),
        title: m.title,
        isbn: m.isbn,
        summary: m.summary,
        publisher: m.publisher,
        publication_year: m.publication_year,
        dewey_decimal: m.dewey_decimal,
        lcc: m.lcc,
        subjects: m.subjects,
        marc_record: m.marc_record,
        cataloguing_notes: m.cataloguing_notes,
        source_data: m.source_data,
        shelf_position: m.shelf_position,
        reading_status: m.reading_status,
        finished_reading_at: m.finished_reading_at,
        started_reading_at: m.started_reading_at,
        cover_url: m.cover_url,
        created_at: Some(m.created_at),
        updated_at: Some(m.updated_at),
        user_rating: m.user_rating,
        owned: m.owned,
        price: m.price,
        digital_formats: m.digital_formats,
        private: m.private,
        page_count: m.page_count,
        loan_duration_days: m.loan_duration_days,
        author: None,
    }
}

fn contact_model_to_import_contact(
    m: crate::models::contact::Model,
) -> crate::api::export::ImportContact {
    crate::api::export::ImportContact {
        id: Some(m.id),
        r#type: m.r#type,
        name: m.name,
        first_name: m.first_name,
        email: m.email,
        phone: m.phone,
        address: m.address,
        street_address: m.street_address,
        postal_code: m.postal_code,
        city: m.city,
        country: m.country,
        latitude: m.latitude,
        longitude: m.longitude,
        notes: m.notes,
        user_id: m.user_id,
        library_owner_id: m.library_owner_id,
        is_active: m.is_active,
        created_at: Some(m.created_at),
        updated_at: Some(m.updated_at),
    }
}

fn tag_model_to_import_tag(m: crate::models::tag::Model) -> crate::api::export::ImportTag {
    crate::api::export::ImportTag {
        id: Some(m.id),
        name: m.name,
        parent_id: m.parent_id,
        path: m.path,
        created_at: Some(m.created_at),
        updated_at: Some(m.updated_at),
    }
}

// -----------------------------------------------------------------------------
// Crypto / zip helpers
// -----------------------------------------------------------------------------

fn parse_manifest_from_zip<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<ManifestSummary, BackupError> {
    let bytes = read_zip_entry_bytes(zip, ENTRY_MANIFEST)?;
    let manifest: ManifestSummary = serde_json::from_slice(&bytes)?;
    Ok(manifest)
}

fn read_zip_entry_bytes<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, BackupError> {
    use std::io::Read;
    let mut entry = zip
        .by_name(name)
        .map_err(|e| BackupError::Zip(format!("missing entry {name}: {e}")))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf)?;
    Ok(buf)
}

fn derive_keys_from_secret(
    secret: &[u8],
    manifest: &ManifestSummary,
) -> Result<([u8; 32], [u8; 32]), BackupError> {
    let salt_vec = B64
        .decode(&manifest.argon2.salt_b64)
        .map_err(|e| BackupError::Crypto(format!("invalid salt b64: {e}")))?;
    let salt: [u8; 32] = salt_vec
        .try_into()
        .map_err(|_| BackupError::Crypto("salt size".into()))?;
    let mut master = derive_key_from_password(secret, &salt)?;
    let pair = derive_subkeys(&master)?;
    master.zeroize();
    Ok(pair)
}

fn check_signature<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    k_mac: &[u8; 32],
    manifest: &ManifestSummary,
) -> Result<(), BackupError> {
    let manifest_bytes = read_zip_entry_bytes(zip, ENTRY_MANIFEST)?;
    let enc_db = read_zip_entry_bytes(zip, ENTRY_DB)?;
    let enc_prefs = read_zip_entry_bytes(zip, ENTRY_PREFS)?;
    let enc_identity = if manifest.identity_included {
        Some(read_zip_entry_bytes(zip, ENTRY_IDENTITY)?)
    } else {
        None
    };

    // Cover entries are HMAC'd in the same sorted order the writer used.
    let mut cover_names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|e| e.name().to_string()))
        .filter(|n| n.starts_with("covers/"))
        .collect();
    cover_names.sort();
    let mut enc_covers: Vec<Vec<u8>> = Vec::with_capacity(cover_names.len());
    for name in &cover_names {
        enc_covers.push(read_zip_entry_bytes(zip, name)?);
    }

    let stored_sig = read_zip_entry_bytes(zip, ENTRY_SIGNATURE)?;

    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(k_mac)
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    mac.update(&manifest_bytes);
    mac.update(&enc_db);
    mac.update(&enc_prefs);
    if let Some(ei) = &enc_identity {
        mac.update(ei);
    }
    for ct in &enc_covers {
        mac.update(ct);
    }
    mac.verify_slice(&stored_sig)
        .map_err(|_| BackupError::BadSignature)
}

fn unseal_entry(key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>, BackupError> {
    if sealed.len() < 12 + 16 {
        return Err(BackupError::Crypto("entry too short".into()));
    }
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&sealed[..12]);
    let ciphertext = &sealed[12..];
    Ok(crate::crypto::encryption::decrypt_aes_gcm(
        key, &nonce, ciphertext,
    )?)
}

fn collect_cover_plaintext<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    k_enc: &[u8; 32],
    manifest: &ManifestSummary,
) -> Result<std::collections::HashMap<String, (Vec<u8>, String)>, BackupError> {
    // Map sha256 -> (plaintext bytes, extension). The manifest already lists
    // the expected covers; iterating it lets us tolerate a zip with more
    // entries (we only consider the manifest-declared ones).
    let mut out: std::collections::HashMap<String, (Vec<u8>, String)> =
        std::collections::HashMap::with_capacity(manifest.covers.len());
    for cover in &manifest.covers {
        // The writer's entry name was `covers/<sha256>.<ext>` (any ext). We
        // probe a few common extensions and fall back to a directory scan.
        let entry_name = find_cover_entry_name(zip, &cover.sha256)?;
        let enc = read_zip_entry_bytes(zip, &entry_name)?;
        let plain = unseal_entry(k_enc, &enc)?;
        // Verify cover plaintext sha256 matches the manifest entry.
        if !ct_eq_str(&sha256_hex(&plain), &cover.sha256) {
            return Err(BackupError::Crypto(format!(
                "cover hash mismatch for {}",
                cover.sha256
            )));
        }
        let ext = entry_name
            .rsplit_once('.')
            .map(|(_, e)| e.to_string())
            .unwrap_or_else(|| "bin".to_string());
        out.insert(cover.sha256.clone(), (plain, ext));
    }
    Ok(out)
}

fn find_cover_entry_name<R: std::io::Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    sha256: &str,
) -> Result<String, BackupError> {
    let prefix = format!("covers/{sha256}.");
    for i in 0..zip.len() {
        if let Ok(entry) = zip.by_index(i) {
            let name = entry.name().to_string();
            if name.starts_with(&prefix) {
                return Ok(name);
            }
        }
    }
    Err(BackupError::Zip(format!("cover entry not found: {sha256}")))
}

fn write_covers(
    cover_dir: &Path,
    plaintext_map: &std::collections::HashMap<String, (Vec<u8>, String)>,
) -> Result<i64, BackupError> {
    let mut n: i64 = 0;
    for (sha, (bytes, ext)) in plaintext_map {
        let dest = cover_dir.join(format!("{sha}.{ext}"));
        std::fs::write(&dest, bytes)?;
        n += 1;
    }
    Ok(n)
}

fn write_covers_additive(
    cover_dir: &Path,
    plaintext_map: &std::collections::HashMap<String, (Vec<u8>, String)>,
) -> Result<i64, BackupError> {
    let mut n: i64 = 0;
    for (sha, (bytes, ext)) in plaintext_map {
        let dest = cover_dir.join(format!("{sha}.{ext}"));
        if dest.exists() {
            continue;
        }
        std::fs::write(&dest, bytes)?;
        n += 1;
    }
    Ok(n)
}

fn wipe_cover_dir(cover_dir: &Path) -> std::io::Result<()> {
    let read = match std::fs::read_dir(cover_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in read {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Filesystem helpers (rename + GC)
// -----------------------------------------------------------------------------

fn sibling_with_suffix(p: &Path, suffix: &str, ts: &chrono::DateTime<chrono::Utc>) -> PathBuf {
    let stamp = ts.format("%Y%m%dT%H%M%SZ").to_string();
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    s.push(stamp);
    PathBuf::from(s)
}

fn sibling_aux(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Rename `from` to `to`, then move the SQLite WAL/SHM siblings alongside if
/// they exist. Both target FS positions must live on the same filesystem as
/// the source so each rename is atomic (POSIX `renameat`); cross-FS errors
/// (`EXDEV`) bubble up rather than triggering a copy+delete fallback.
fn rename_db_with_wal(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)?;
    let wal_from = sibling_aux(from, "-wal");
    let wal_to = sibling_aux(to, "-wal");
    if wal_from.exists() {
        let _ = std::fs::rename(&wal_from, &wal_to);
    }
    let shm_from = sibling_aux(from, "-shm");
    let shm_to = sibling_aux(to, "-shm");
    if shm_from.exists() {
        let _ = std::fs::rename(&shm_from, &shm_to);
    }
    Ok(())
}

fn parse_rollback_timestamp(suffix: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Format from `sibling_with_suffix`: `YYYYMMDDTHHMMSSZ`.
    let parsed = chrono::NaiveDateTime::parse_from_str(suffix, "%Y%m%dT%H%M%SZ").ok()?;
    Some(parsed.and_utc())
}

/// Equality on hex SHA-256 strings. Both values are non-secret (the manifest
/// is in clear and the plaintext hash is recomputed from public bytes), so a
/// constant-time comparison would not add real protection here. The HMAC on
/// the archive itself, which IS keyed and security-critical, goes through
/// `Mac::verify_slice` which is constant-time internally.
fn ct_eq_str(a: &str, b: &str) -> bool {
    a == b
}

/// Garbage-collect rollback / replaced siblings older than 24h. Called by
/// `infrastructure::db::run_startup_maintenance` at app launch.
pub fn purge_expired_rollbacks(db_path: &Path) {
    let parent = match db_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };
    let Some(base) = db_path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let prefixes = [
        format!("{base}{ROLLBACK_SUFFIX}"),
        format!("{base}{REPLACED_SUFFIX}"),
    ];
    let now = chrono::Utc::now();
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&parent) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    for entry in entries {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some((stamp, _)) = prefixes
            .iter()
            .find_map(|p| name_str.strip_prefix(p).map(|s| (s, p)))
        else {
            continue;
        };
        let Some(ts) = parse_rollback_timestamp(stamp) else {
            continue;
        };
        let age = (now - ts).num_seconds();
        let path = entry.path();
        if age > ROLLBACK_TTL_SECONDS {
            if std::fs::remove_file(&path).is_ok() {
                let _ = std::fs::remove_file(sibling_aux(&path, "-wal"));
                let _ = std::fs::remove_file(sibling_aux(&path, "-shm"));
                tracing::info!(
                    path = %path.display(),
                    age_seconds = age,
                    "purged expired rollback"
                );
            }
        } else {
            tracing::debug!(
                path = %path.display(),
                age_seconds = age,
                "kept rollback"
            );
        }
    }
}

/// Returns the highest `updated_at` across the four user-facing tables that
/// together represent a catalog change: `books`, `copies`, `loans`, and
/// `library_config`. Returns `None` when all four tables are empty (fresh
/// install before any seeding).
///
/// Used by the auto-backup scheduler (ADR-037 §6) as the watermark for the
/// "skip-if-unchanged" check. ISO 8601 timestamps are lexicographically
/// ordered, so plain `MAX(...)` is sufficient; no dedicated index is needed
/// (sub-millisecond on realistic library sizes).
pub async fn latest_user_data_change_at(
    db: &DatabaseConnection,
) -> Result<Option<String>, BackupError> {
    use sea_orm::Statement;
    let stmt = Statement::from_string(
        db.get_database_backend(),
        "SELECT MAX(updated_at) AS m FROM (\
             SELECT updated_at FROM books \
             UNION ALL SELECT updated_at FROM copies \
             UNION ALL SELECT updated_at FROM loans \
             UNION ALL SELECT updated_at FROM library_config\
         )"
        .to_owned(),
    );
    let row = db
        .query_one(stmt)
        .await
        .map_err(|e| BackupError::Db(e.to_string()))?;
    match row {
        Some(r) => r
            .try_get::<Option<String>>("", "m")
            .map_err(|e| BackupError::Db(e.to_string())),
        None => Ok(None),
    }
}
