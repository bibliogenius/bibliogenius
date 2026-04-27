//! Identity Service — persistence and lifecycle of the node's cryptographic identity.
//!
//! Generates a `NodeIdentity` once, encrypts the secret keys with Argon2(library_uuid),
//! and stores them in the `crypto_keys` table. Subsequent calls reload and decrypt.
//!
//! Thread-safe: the identity is initialized at most once via `tokio::sync::OnceCell`.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::sync::Arc;
use tokio::sync::OnceCell;
use zeroize::Zeroize;

use crate::crypto::encryption::{
    decrypt_aes_gcm, derive_key_from_password, encrypt_aes_gcm, generate_salt, zeroize_key,
};
use crate::crypto::identity::NodeIdentity;

/// Stable prefix surfaced to Flutter so the recovery dialog can pattern-match
/// on the FFI exception message without parsing free-form text.
pub const E_IDENTITY_DECRYPT_FAILED: &str = "E_IDENTITY_DECRYPT_FAILED";

/// Errors returned by `IdentityService::init`.
///
/// `DecryptionFailed` signals that the stored `crypto_keys` row exists but cannot
/// be decrypted with the provided `library_uuid` (typically a storage swing
/// between Keychain and NSUserDefaults on macOS, see
/// `memory/e2ee_identity_storage_fragility.md`). Callers MUST surface a recovery
/// flow to the user (retry / explicit regeneration) instead of silently wiping
/// the keys, because regeneration breaks every paired peer.
#[derive(Debug)]
pub enum IdentityError {
    DecryptionFailed(String),
    Other(String),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecryptionFailed(detail) => write!(f, "{E_IDENTITY_DECRYPT_FAILED}: {detail}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<String> for IdentityError {
    fn from(msg: String) -> Self {
        Self::Other(msg)
    }
}

/// Thread-safe identity service. Ensures generate-once semantics.
#[derive(Clone)]
pub struct IdentityService {
    db: DatabaseConnection,
    identity: Arc<OnceCell<NodeIdentity>>,
    /// The library UUID used to initialize the identity (stable P2P identifier).
    uuid: Arc<OnceCell<String>>,
}

impl IdentityService {
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            identity: Arc::new(OnceCell::new()),
            uuid: Arc::new(OnceCell::new()),
        }
    }

    /// Initialize or reload the node identity.
    ///
    /// - If `crypto_keys` has existing keys for user_id=0, decrypt and load them.
    /// - Otherwise, generate a fresh identity, encrypt, and store.
    /// - If the keys exist but cannot be decrypted, return
    ///   `IdentityError::DecryptionFailed`. Callers MUST surface a recovery flow
    ///   (the keys are intentionally NOT auto-wiped — regeneration breaks every
    ///   paired peer and must be a deliberate, user-confirmed action).
    ///
    /// Uses Argon2id(library_uuid) as the encryption password.
    pub async fn init(&self, library_uuid: &str) -> Result<(), IdentityError> {
        let db = &self.db;
        let uuid = library_uuid.to_string();

        // Store the library UUID for later retrieval by handlers
        let _ = self.uuid.set(uuid.clone());

        self.identity
            .get_or_try_init(|| async {
                // Try to load existing keys
                match load_identity_from_db(db, &uuid).await {
                    Ok(Some(identity)) => {
                        tracing::info!("Loaded existing node identity from crypto_keys");
                        Ok(identity)
                    }
                    Ok(None) => {
                        // Generate fresh identity
                        let identity = NodeIdentity::generate();
                        store_identity_to_db(db, &identity, &uuid).await?;
                        tracing::info!("Generated and stored new node identity");
                        Ok(identity)
                    }
                    Err(e) if e.contains("Decryption failed") || e.contains("not 32 bytes") => {
                        // Stored keys cannot be decrypted with the supplied
                        // library_uuid. Most often this is a storage swing
                        // (Keychain ↔ NSUserDefaults on macOS) — the original
                        // UUID is still recoverable on the device, so we MUST
                        // NOT wipe the keys here. Bubble the typed error up;
                        // the FFI surface translates it to a stable string the
                        // Flutter layer matches on to show a recovery dialog.
                        tracing::error!(
                            "Stored identity undecryptable ({e}); refusing to silently regenerate"
                        );
                        Err(IdentityError::DecryptionFailed(e))
                    }
                    Err(e) => Err(IdentityError::Other(e)),
                }
            })
            .await?;

        Ok(())
    }

    /// Returns (ed25519_hex, x25519_hex) public keys.
    pub fn get_public_keys_hex(&self) -> Result<(String, String), String> {
        let identity = self
            .identity
            .get()
            .ok_or_else(|| "Identity not initialized".to_string())?;

        let ed25519_hex = hex::encode(identity.verifying_key().as_bytes());
        let x25519_hex = hex::encode(identity.x25519_public_key().as_bytes());

        Ok((ed25519_hex, x25519_hex))
    }

    /// Access the underlying NodeIdentity (for CryptoService in Phase 3+).
    pub fn identity(&self) -> Result<&NodeIdentity, String> {
        self.identity
            .get()
            .ok_or_else(|| "Identity not initialized".to_string())
    }

    /// Returns the library UUID used to initialize this identity.
    /// Available after `init()` has been called.
    pub fn library_uuid(&self) -> Option<&str> {
        self.uuid.get().map(|s| s.as_str())
    }
}

/// Load identity from crypto_keys table (user_id=1, key_type in {ed25519, x25519}).
async fn load_identity_from_db(
    db: &DatabaseConnection,
    library_uuid: &str,
) -> Result<Option<NodeIdentity>, String> {
    // Query both key rows
    let rows = db
        .query_all(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT key_type, public_key, encrypted_secret, salt FROM crypto_keys WHERE user_id = 0 AND revoked_at IS NULL ORDER BY key_type",
            [],
        ))
        .await
        .map_err(|e| format!("DB query failed: {e}"))?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut ed25519_data: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = None;
    let mut x25519_data: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = None;

    for row in &rows {
        let key_type: String = row
            .try_get("", "key_type")
            .map_err(|e| format!("Failed to read key_type: {e}"))?;
        let _public_key: Vec<u8> = row
            .try_get("", "public_key")
            .map_err(|e| format!("Failed to read public_key: {e}"))?;
        let encrypted_secret: Vec<u8> = row
            .try_get("", "encrypted_secret")
            .map_err(|e| format!("Failed to read encrypted_secret: {e}"))?;
        let salt: Vec<u8> = row
            .try_get("", "salt")
            .map_err(|e| format!("Failed to read salt: {e}"))?;

        match key_type.as_str() {
            "ed25519" => ed25519_data = Some((_public_key, encrypted_secret, salt)),
            "x25519" => x25519_data = Some((_public_key, encrypted_secret, salt)),
            _ => {} // ignore unknown key types
        }
    }

    let (Some(ed_data), Some(x_data)) = (ed25519_data, x25519_data) else {
        // Partial data — treat as missing
        return Ok(None);
    };

    // Decrypt Ed25519 secret
    let mut ed_key = derive_argon2_key(library_uuid, &ed_data.2)?;
    let ed_secret = decrypt_secret(&ed_key, &ed_data.1)?;
    zeroize_key(&mut ed_key);

    // Decrypt X25519 secret
    let mut x_key = derive_argon2_key(library_uuid, &x_data.2)?;
    let x_secret = decrypt_secret(&x_key, &x_data.1)?;
    zeroize_key(&mut x_key);

    let ed_bytes: [u8; 32] = ed_secret
        .try_into()
        .map_err(|_| "Ed25519 secret not 32 bytes".to_string())?;
    let x_bytes: [u8; 32] = x_secret
        .try_into()
        .map_err(|_| "X25519 secret not 32 bytes".to_string())?;

    let identity = NodeIdentity::from_bytes(&ed_bytes, &x_bytes);

    // Zeroize the decrypted bytes (stack arrays)
    let mut ed_bytes_mut = ed_bytes;
    let mut x_bytes_mut = x_bytes;
    ed_bytes_mut.zeroize();
    x_bytes_mut.zeroize();

    Ok(Some(identity))
}

/// Store identity in crypto_keys table (2 rows: ed25519 + x25519).
async fn store_identity_to_db(
    db: &DatabaseConnection,
    identity: &NodeIdentity,
    library_uuid: &str,
) -> Result<(), String> {
    let (mut ed_secret, mut x_secret) = identity.export_secret_bytes();

    // Encrypt Ed25519
    let ed_salt = generate_salt();
    let mut ed_key = derive_argon2_key(library_uuid, &ed_salt)?;
    let ed_encrypted = encrypt_secret(&ed_key, &ed_secret)?;
    zeroize_key(&mut ed_key);
    ed_secret.zeroize();

    // Encrypt X25519
    let x_salt = generate_salt();
    let mut x_key = derive_argon2_key(library_uuid, &x_salt)?;
    let x_encrypted = encrypt_secret(&x_key, &x_secret)?;
    zeroize_key(&mut x_key);
    x_secret.zeroize();

    let ed25519_public = identity.verifying_key().as_bytes().to_vec();
    let x25519_public = identity.x25519_public_key().as_bytes().to_vec();

    // Insert Ed25519 row
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO crypto_keys (user_id, key_type, public_key, encrypted_secret, salt) VALUES (0, 'ed25519', $1, $2, $3)",
        [ed25519_public.into(), ed_encrypted.into(), ed_salt.to_vec().into()],
    ))
    .await
    .map_err(|e| format!("Failed to store ed25519 key: {e}"))?;

    // Insert X25519 row
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO crypto_keys (user_id, key_type, public_key, encrypted_secret, salt) VALUES (0, 'x25519', $1, $2, $3)",
        [x25519_public.into(), x_encrypted.into(), x_salt.to_vec().into()],
    ))
    .await
    .map_err(|e| format!("Failed to store x25519 key: {e}"))?;

    Ok(())
}

/// Delete all identity keys from `crypto_keys`.
///
/// Used by:
/// - `reset_app` (full reset path: wipes the row so the next init regenerates cleanly)
/// - `confirm_regenerate_identity_ffi` (user-confirmed recovery from `DecryptionFailed`)
pub(crate) async fn delete_identity_from_db(db: &DatabaseConnection) -> Result<(), String> {
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "DELETE FROM crypto_keys WHERE user_id = 0",
        [],
    ))
    .await
    .map_err(|e| format!("Failed to delete old crypto_keys: {e}"))?;
    Ok(())
}

/// Derive Argon2 key from library_uuid + salt.
fn derive_argon2_key(library_uuid: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let salt_array: [u8; 32] = salt
        .try_into()
        .map_err(|_| "Salt must be 32 bytes".to_string())?;
    derive_key_from_password(library_uuid.as_bytes(), &salt_array)
        .map_err(|e| format!("Argon2 key derivation failed: {e}"))
}

/// Encrypt a 32-byte secret with AES-256-GCM. Returns nonce || ciphertext.
fn encrypt_secret(key: &[u8; 32], secret: &[u8; 32]) -> Result<Vec<u8>, String> {
    let (nonce, ciphertext) =
        encrypt_aes_gcm(key, secret).map_err(|e| format!("Encryption failed: {e}"))?;
    let mut result = Vec::with_capacity(12 + ciphertext.len());
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Decrypt a secret. Input is nonce (12 bytes) || ciphertext.
fn decrypt_secret(key: &[u8; 32], encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if encrypted.len() < 12 {
        return Err("Encrypted data too short".to_string());
    }
    let nonce: [u8; 12] = encrypted[..12]
        .try_into()
        .map_err(|_| "Invalid nonce".to_string())?;
    let ciphertext = &encrypted[12..];
    decrypt_aes_gcm(key, &nonce, ciphertext).map_err(|e| format!("Decryption failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    async fn setup_test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db)
            .await
            .unwrap();

        db
    }

    #[tokio::test]
    async fn identity_generate_store_reload_roundtrip() {
        let db = setup_test_db().await;
        let uuid = "test-library-uuid-1234";

        // First init → generates new identity
        let svc1 = IdentityService::new(db.clone());
        svc1.init(uuid).await.unwrap();
        let (ed1, x1) = svc1.get_public_keys_hex().unwrap();

        // Second init on fresh service → should reload same keys
        let svc2 = IdentityService::new(db.clone());
        svc2.init(uuid).await.unwrap();
        let (ed2, x2) = svc2.get_public_keys_hex().unwrap();

        assert_eq!(
            ed1, ed2,
            "Ed25519 public key should be the same after reload"
        );
        assert_eq!(x1, x2, "X25519 public key should be the same after reload");
    }

    #[tokio::test]
    async fn wrong_uuid_returns_error() {
        let db = setup_test_db().await;

        // Store with one UUID
        let svc1 = IdentityService::new(db.clone());
        svc1.init("correct-uuid").await.unwrap();
        let rows_before = count_crypto_keys(&db).await;
        assert_eq!(
            rows_before, 2,
            "expected 2 crypto_keys rows after first init (ed25519 + x25519)"
        );

        // Init with a different UUID: stored keys cannot be decrypted.
        // Pre-fix: this would silently DELETE crypto_keys + regenerate.
        // Post-fix: it must return DecryptionFailed and leave the row intact
        // so the original UUID can still recover the keys (e.g. after a
        // Keychain ↔ NSUserDefaults swing).
        let svc2 = IdentityService::new(db.clone());
        let err = svc2
            .init("wrong-uuid")
            .await
            .expect_err("init must fail when UUID doesn't match stored keys");

        assert!(
            matches!(err, IdentityError::DecryptionFailed(_)),
            "expected IdentityError::DecryptionFailed, got {err:?}"
        );
        assert!(
            err.to_string().starts_with(E_IDENTITY_DECRYPT_FAILED),
            "Display must emit the stable prefix for Flutter to pattern-match, got {err}"
        );

        let rows_after = count_crypto_keys(&db).await;
        assert_eq!(
            rows_after, rows_before,
            "crypto_keys must remain intact on DecryptionFailed (no silent wipe)"
        );

        // The original UUID still works — proves the row was preserved.
        let svc3 = IdentityService::new(db.clone());
        svc3.init("correct-uuid").await.unwrap();
    }

    #[tokio::test]
    async fn delete_identity_from_db_clears_crypto_keys() {
        // Covers the contract relied upon by reset_app and
        // confirm_regenerate_identity_ffi: wiping the row leaves crypto_keys
        // empty, so the next init takes the Ok(None) branch and regenerates
        // a fresh identity without going through the DecryptionFailed path.
        let db = setup_test_db().await;

        let svc = IdentityService::new(db.clone());
        svc.init("some-uuid").await.unwrap();
        assert_eq!(count_crypto_keys(&db).await, 2);

        delete_identity_from_db(&db).await.unwrap();
        assert_eq!(count_crypto_keys(&db).await, 0);

        // Next init on a fresh service falls through Ok(None) and regenerates.
        let svc2 = IdentityService::new(db.clone());
        svc2.init("brand-new-uuid").await.unwrap();
        assert_eq!(count_crypto_keys(&db).await, 2);
    }

    async fn count_crypto_keys(db: &DatabaseConnection) -> i64 {
        let row = db
            .query_one(Statement::from_sql_and_values(
                db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM crypto_keys WHERE user_id = 0",
                [],
            ))
            .await
            .unwrap()
            .unwrap();
        row.try_get::<i64>("", "n").unwrap()
    }

    #[tokio::test]
    async fn public_keys_are_valid_hex() {
        let db = setup_test_db().await;
        let svc = IdentityService::new(db);
        svc.init("test-uuid").await.unwrap();

        let (ed, x) = svc.get_public_keys_hex().unwrap();
        assert_eq!(ed.len(), 64, "Ed25519 hex should be 64 chars");
        assert_eq!(x.len(), 64, "X25519 hex should be 64 chars");
        assert!(hex::decode(&ed).is_ok());
        assert!(hex::decode(&x).is_ok());
    }

    #[test]
    fn qr_v2_generate_parse_roundtrip() {
        let payload = serde_json::json!({
            "version": 2,
            "name": "My Library",
            "url": "http://192.168.1.42:8000",
            "library_uuid": "550e8400-e29b-41d4-a716-446655440000",
            "ed25519_public_key": "a".repeat(64),
            "x25519_public_key": "b".repeat(64),
        });

        let json_str = payload.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["version"], 2);
        assert_eq!(parsed["name"], "My Library");
        assert_eq!(parsed["url"], "http://192.168.1.42:8000");
        assert_eq!(parsed["ed25519_public_key"].as_str().unwrap().len(), 64);
        assert_eq!(parsed["x25519_public_key"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn qr_v1_backward_compat() {
        // QR v1 has no version field, just name + url
        let v1_payload = r#"{"name": "Old Library", "url": "http://10.0.0.5:8000"}"#;
        let parsed: serde_json::Value = serde_json::from_str(v1_payload).unwrap();

        let version = parsed.get("version").and_then(|v| v.as_i64()).unwrap_or(1);
        assert_eq!(version, 1);
        assert_eq!(parsed["name"], "Old Library");
        assert_eq!(parsed["url"], "http://10.0.0.5:8000");
        assert!(parsed.get("ed25519_public_key").is_none());
    }

    #[test]
    fn invite_link_generate_parse_roundtrip() {
        use base64::Engine;

        let payload = serde_json::json!({
            "version": 2,
            "name": "Test Lib",
            "url": "http://192.168.1.10:8000",
            "library_uuid": "test-uuid-1234",
            "ed25519_public_key": "c".repeat(64),
            "x25519_public_key": "d".repeat(64),
        });

        // Generate
        let json_bytes = payload.to_string().into_bytes();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json_bytes);
        let link = format!("https://bibliogenius.app/invite#{encoded}");

        // Parse
        let fragment = link.split_once('#').unwrap().1;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(fragment)
            .unwrap();
        let json_str = String::from_utf8(decoded).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed["version"], 2);
        assert_eq!(parsed["name"], "Test Lib");
        assert_eq!(parsed["url"], "http://192.168.1.10:8000");
        assert_eq!(parsed["library_uuid"], "test-uuid-1234");
        assert_eq!(parsed["ed25519_public_key"].as_str().unwrap().len(), 64);

        // Fragment should never be sent to the server (B8)
        assert!(link.starts_with("https://bibliogenius.app/invite#"));
    }
}
