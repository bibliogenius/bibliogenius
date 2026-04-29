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
