//! Client at-rest persistence of the account session (ST-05 Phase F, ADR-042 §14
//! client-persistence addendum).
//!
//! After a device enrolls into an account (passphrase or sealed path), the unlocked
//! trousseau must survive app restarts WITHOUT re-enrolling — re-deriving it would
//! demand the passphrase every launch, and the sealed path is unrepeatable (the sealing
//! device is gone). So the trousseau is sealed AT REST under the same
//! `Argon2(library_uuid)` device-local key that protects `crypto_keys`, and stored in the
//! singleton `account_session` row (migration 081) next to the opaque hub `account_id`,
//! the login `email`, and this device's random `device_id` lane key.
//!
//! The trousseau plaintext never touches the database in the clear: sealing happens
//! inside [`AccountKeyBundle::seal_at_rest`] and the 96-byte layout stays in the crypto
//! module. The device-local key is zeroized immediately after use (A1), and nothing here
//! is ever logged (A2).

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::crypto::account_keys::AccountKeyBundle;
use crate::crypto::encryption::{derive_key_from_password, generate_salt, zeroize_key};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rand::rngs::OsRng;

/// A persisted account session reloaded from disk: everything a sync cycle needs except
/// the live hub client (which re-authenticates from the trousseau via login).
pub struct PersistedSession {
    /// Opaque hub account id (bound into blob AAD, keys `account_sync_state`).
    pub account_id: String,
    /// The email the account authenticates under (login / bootstrap).
    pub email: String,
    /// This device's random base64url lane key (`SyncContext.device_id`, `DeviceEntry.device_id`).
    pub device_id: String,
    /// The unlocked trousseau, decrypted only in RAM (never logged, zeroized on drop).
    pub bundle: AccountKeyBundle,
}

#[derive(Debug)]
pub enum SessionStoreError {
    /// The stored row exists but its trousseau could not be decrypted with this device's
    /// `library_uuid` (e.g. a Keychain↔NSUserDefaults swing, see the identity fragility note).
    DecryptionFailed,
    /// Crypto failure deriving the device-local key or sealing the trousseau.
    Crypto(String),
    /// Database or encoding failure.
    Storage(String),
}

impl std::fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecryptionFailed => write!(f, "Stored account session could not be decrypted"),
            Self::Crypto(e) => write!(f, "Crypto error: {e}"),
            Self::Storage(e) => write!(f, "Storage error: {e}"),
        }
    }
}

impl std::error::Error for SessionStoreError {}

/// Generate a fresh random 256-bit device lane key, base64url(no-pad) (ADR-042 §13.5).
/// Random — never derived from the device's public identity — so the blind hub cannot
/// correlate an account's lanes to a publicly listed library.
pub fn generate_device_id() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Derive the device-local wrapping key on the blocking pool. The device-local KDF is
/// Argon2id 64 MiB (B7); run inline it would stall the single-threaded FFI runtime (same
/// hazard the account KDF avoids in `account_enrollment`, pattern from `api/backup.rs`), so
/// it goes through `spawn_blocking`. `salt` is `Copy`, so callers keep their own copy.
async fn derive_device_local_key(
    library_uuid: &str,
    salt: [u8; 32],
) -> Result<[u8; 32], SessionStoreError> {
    let lib = library_uuid.to_string();
    tokio::task::spawn_blocking(move || derive_key_from_password(lib.as_bytes(), &salt))
        .await
        .map_err(|e| SessionStoreError::Crypto(format!("key derivation task failed: {e}")))?
        .map_err(|e| SessionStoreError::Crypto(e.to_string()))
}

/// Persist (or replace) the account session, sealing the trousseau at rest under the
/// `Argon2(library_uuid)` device-local key. One session per device (singleton row): any
/// previous session is overwritten.
pub async fn persist(
    db: &DatabaseConnection,
    library_uuid: &str,
    account_id: &str,
    email: &str,
    device_id: &str,
    bundle: &AccountKeyBundle,
) -> Result<(), SessionStoreError> {
    let salt = generate_salt();
    let mut key = derive_device_local_key(library_uuid, salt).await?;
    let sealed = bundle
        .seal_at_rest(&key)
        .map_err(|e| SessionStoreError::Crypto(e.to_string()));
    zeroize_key(&mut key);
    let sealed = sealed?;

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO account_session (id, account_id, email, device_id, encrypted_bundle, salt) \
         VALUES (0, ?, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
           account_id = excluded.account_id, email = excluded.email, \
           device_id = excluded.device_id, encrypted_bundle = excluded.encrypted_bundle, \
           salt = excluded.salt, created_at = datetime('now')",
        [
            account_id.into(),
            email.into(),
            device_id.into(),
            sealed.into(),
            salt.to_vec().into(),
        ],
    ))
    .await
    .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    Ok(())
}

/// Reload the persisted session, decrypting the trousseau with the device-local key.
/// `Ok(None)` if no session is stored; [`SessionStoreError::DecryptionFailed`] if the row
/// exists but the `library_uuid` no longer unlocks it (the caller surfaces recovery, never
/// a silent wipe — matching `IdentityService`).
pub async fn load(
    db: &DatabaseConnection,
    library_uuid: &str,
) -> Result<Option<PersistedSession>, SessionStoreError> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT account_id, email, device_id, encrypted_bundle, salt \
             FROM account_session WHERE id = 0"
                .to_owned(),
        ))
        .await
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let Some(row) = row else {
        return Ok(None);
    };

    let account_id: String = row
        .try_get("", "account_id")
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let email: String = row
        .try_get("", "email")
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let device_id: String = row
        .try_get("", "device_id")
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let encrypted_bundle: Vec<u8> = row
        .try_get("", "encrypted_bundle")
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let salt_vec: Vec<u8> = row
        .try_get("", "salt")
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let salt: [u8; 32] = salt_vec
        .try_into()
        .map_err(|_| SessionStoreError::Storage("salt must be 32 bytes".to_string()))?;

    let mut key = derive_device_local_key(library_uuid, salt).await?;
    let opened = AccountKeyBundle::open_at_rest(&key, &encrypted_bundle);
    zeroize_key(&mut key);
    let bundle = opened.map_err(|_| SessionStoreError::DecryptionFailed)?;

    Ok(Some(PersistedSession {
        account_id,
        email,
        device_id,
        bundle,
    }))
}

/// Light session metadata: the plaintext columns, read WITHOUT decrypting the trousseau.
/// For the UI status surface, which must not pay the Argon2 unlock cost just to render.
pub struct SessionMetadata {
    pub account_id: String,
    pub email: String,
    pub device_id: String,
}

/// Read the persisted session's plaintext metadata (no trousseau decryption). `Ok(None)`
/// if no session is stored.
pub async fn load_metadata(
    db: &DatabaseConnection,
) -> Result<Option<SessionMetadata>, SessionStoreError> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT account_id, email, device_id FROM account_session WHERE id = 0".to_owned(),
        ))
        .await
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let get = |c: &str| -> Result<String, SessionStoreError> {
        row.try_get("", c)
            .map_err(|e| SessionStoreError::Storage(e.to_string()))
    };
    Ok(Some(SessionMetadata {
        account_id: get("account_id")?,
        email: get("email")?,
        device_id: get("device_id")?,
    }))
}

/// Whether an account session is currently persisted (cheap existence probe for the UI).
pub async fn exists(db: &DatabaseConnection) -> Result<bool, SessionStoreError> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT 1 AS present FROM account_session WHERE id = 0".to_owned(),
        ))
        .await
        .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    Ok(row.is_some())
}

/// Drop the persisted session (logout). Idempotent.
pub async fn clear(db: &DatabaseConnection) -> Result<(), SessionStoreError> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM account_session WHERE id = 0".to_owned(),
    ))
    .await
    .map_err(|e| SessionStoreError::Storage(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;

    async fn setup_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db)
            .await
            .unwrap();
        db
    }

    #[test]
    fn device_ids_are_random_and_url_safe() {
        let a = generate_device_id();
        let b = generate_device_id();
        assert_ne!(a, b, "device ids must not collide");
        // 32 bytes base64url(no-pad) = 43 chars, no padding, url-safe alphabet only.
        assert_eq!(a.len(), 43);
        assert!(URL_SAFE_NO_PAD.decode(&a).unwrap().len() == 32);
    }

    #[tokio::test]
    async fn persist_load_roundtrip() {
        let db = setup_db().await;
        let uuid = "lib-uuid-1";
        let bundle = AccountKeyBundle::generate();
        let device_id = generate_device_id();

        assert!(load(&db, uuid).await.unwrap().is_none());
        assert!(!exists(&db).await.unwrap());

        persist(&db, uuid, "acct-1", "r@e.org", &device_id, &bundle)
            .await
            .unwrap();
        assert!(exists(&db).await.unwrap());

        let loaded = load(&db, uuid).await.unwrap().expect("session present");
        assert_eq!(loaded.account_id, "acct-1");
        assert_eq!(loaded.email, "r@e.org");
        assert_eq!(loaded.device_id, device_id);
        assert_eq!(loaded.bundle.account_auth_pk(), bundle.account_auth_pk());
    }

    #[tokio::test]
    async fn wrong_library_uuid_fails_decryption_without_wiping() {
        let db = setup_db().await;
        let bundle = AccountKeyBundle::generate();
        persist(&db, "correct-uuid", "acct-1", "r@e.org", "devid", &bundle)
            .await
            .unwrap();

        // A storage swing changes the library_uuid: the row must NOT silently decrypt.
        // (PersistedSession holds the secret trousseau and has no Debug, so match
        // rather than unwrap_err on the Ok variant.)
        match load(&db, "wrong-uuid").await {
            Err(SessionStoreError::DecryptionFailed) => {}
            other => panic!(
                "expected DecryptionFailed, got a different outcome: {:?}",
                other.is_ok()
            ),
        }
        // The row is preserved so the correct uuid can still recover it.
        assert!(exists(&db).await.unwrap());
        assert!(load(&db, "correct-uuid").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn persist_replaces_previous_session() {
        let db = setup_db().await;
        let uuid = "lib-uuid-2";
        let first = AccountKeyBundle::generate();
        let second = AccountKeyBundle::generate();

        persist(&db, uuid, "acct-1", "a@e.org", "dev-a", &first)
            .await
            .unwrap();
        persist(&db, uuid, "acct-2", "b@e.org", "dev-b", &second)
            .await
            .unwrap();

        let loaded = load(&db, uuid).await.unwrap().unwrap();
        assert_eq!(loaded.account_id, "acct-2");
        assert_eq!(loaded.device_id, "dev-b");
        assert_eq!(loaded.bundle.account_auth_pk(), second.account_auth_pk());
    }

    #[tokio::test]
    async fn clear_removes_session() {
        let db = setup_db().await;
        let bundle = AccountKeyBundle::generate();
        persist(&db, "u", "acct-1", "r@e.org", "devid", &bundle)
            .await
            .unwrap();
        clear(&db).await.unwrap();
        assert!(!exists(&db).await.unwrap());
        assert!(load(&db, "u").await.unwrap().is_none());
        // Idempotent.
        clear(&db).await.unwrap();
    }
}
