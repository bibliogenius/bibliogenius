//! At-rest store of the library's directory catalog key (ADR-072 D2).
//!
//! The key lives in `crypto_keys` next to the identity keys, under the same
//! protection: encrypted with an Argon2id key derived from the library uuid
//! and a per-row salt, never in shared preferences. `key_type = 'catalog'`,
//! `public_key` holds the clear key id (it names the key in every
//! ciphertext and wrap, it is not secret), `encrypted_secret` the key bytes.
//! A rotation (D4) marks the current row `revoked_at` and inserts a new one,
//! so the history of key ids stays readable while no old key is ever used
//! to encrypt again.
//!
//! The decrypted key is cached in memory after the first load: the Argon2
//! derivation costs tens of milliseconds and the catalog push runs on every
//! launch and behind every edit. The cache holds a zeroizing [`CatalogKey`]
//! and is dropped on reset.

use std::sync::{Arc, Mutex};

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use zeroize::Zeroize;

use crate::crypto::catalog_key::CatalogKey;
use crate::crypto::encryption::{generate_salt, zeroize_key};

use super::identity_service::{decrypt_secret, derive_argon2_key, encrypt_secret};

const KEY_TYPE: &str = "catalog";

#[derive(Default)]
pub struct CatalogKeyStore {
    cache: Mutex<Option<Arc<CatalogKey>>>,
}

impl CatalogKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current catalog key, when the library already has one. Never
    /// creates one: a library that pushes in clear has no reason to hold a
    /// key, and wrapping a key nobody uses would only add rows on the hub.
    pub async fn load(
        &self,
        db: &DatabaseConnection,
        library_uuid: &str,
    ) -> Result<Option<Arc<CatalogKey>>, String> {
        if let Some(key) = self.cached() {
            return Ok(Some(key));
        }
        let Some(key) = load_current_from_db(db, library_uuid).await? else {
            return Ok(None);
        };
        Ok(Some(self.remember(key)))
    }

    /// The current catalog key, generated and stored on first use.
    pub async fn load_or_create(
        &self,
        db: &DatabaseConnection,
        library_uuid: &str,
    ) -> Result<Arc<CatalogKey>, String> {
        if let Some(key) = self.load(db, library_uuid).await? {
            return Ok(key);
        }
        let key = CatalogKey::generate();
        store_to_db(db, library_uuid, &key).await?;
        Ok(self.remember(key))
    }

    /// Retires the current key and returns a fresh one (ADR-072 D4). The
    /// caller re-encrypts the catalog and re-wraps the new key for the
    /// readers that remain; the ex-reader never receives it.
    pub async fn rotate(
        &self,
        db: &DatabaseConnection,
        library_uuid: &str,
    ) -> Result<Arc<CatalogKey>, String> {
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE crypto_keys SET revoked_at = ? WHERE user_id = 0 AND key_type = ? AND revoked_at IS NULL",
            [chrono::Utc::now().to_rfc3339().into(), KEY_TYPE.into()],
        ))
        .await
        .map_err(|e| format!("Failed to retire catalog key: {e}"))?;
        let key = CatalogKey::generate();
        store_to_db(db, library_uuid, &key).await?;
        Ok(self.remember(key))
    }

    /// Drops the cached key. The rows stay: a reset that wipes the identity
    /// wipes them with the same statement.
    pub fn forget(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    fn cached(&self) -> Option<Arc<CatalogKey>> {
        self.cache.lock().ok().and_then(|c| c.clone())
    }

    fn remember(&self, key: CatalogKey) -> Arc<CatalogKey> {
        let key = Arc::new(key);
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(Arc::clone(&key));
        }
        key
    }
}

async fn load_current_from_db(
    db: &DatabaseConnection,
    library_uuid: &str,
) -> Result<Option<CatalogKey>, String> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT public_key, encrypted_secret, salt FROM crypto_keys
             WHERE user_id = 0 AND key_type = ? AND revoked_at IS NULL
             ORDER BY id DESC LIMIT 1",
            [KEY_TYPE.into()],
        ))
        .await
        .map_err(|e| format!("DB query failed: {e}"))?;
    let Some(row) = row else {
        return Ok(None);
    };

    let key_id: Vec<u8> = row
        .try_get("", "public_key")
        .map_err(|e| format!("Failed to read key id: {e}"))?;
    let encrypted: Vec<u8> = row
        .try_get("", "encrypted_secret")
        .map_err(|e| format!("Failed to read encrypted key: {e}"))?;
    let salt: Vec<u8> = row
        .try_get("", "salt")
        .map_err(|e| format!("Failed to read salt: {e}"))?;

    let key_id: [u8; 16] = key_id
        .try_into()
        .map_err(|_| "Catalog key id not 16 bytes".to_string())?;
    let mut wrapping = derive_argon2_key(library_uuid, &salt)?;
    let secret = decrypt_secret(&wrapping, &encrypted);
    zeroize_key(&mut wrapping);
    let mut secret = secret?;
    let key: [u8; 32] = secret
        .as_slice()
        .try_into()
        .map_err(|_| "Catalog key not 32 bytes".to_string())?;
    secret.zeroize();

    Ok(Some(CatalogKey::from_parts(key_id, key)))
}

async fn store_to_db(
    db: &DatabaseConnection,
    library_uuid: &str,
    key: &CatalogKey,
) -> Result<(), String> {
    let salt = generate_salt();
    let mut wrapping = derive_argon2_key(library_uuid, &salt)?;
    let encrypted = encrypt_secret(&wrapping, &key.export_secret());
    zeroize_key(&mut wrapping);
    let encrypted = encrypted?;

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO crypto_keys (user_id, key_type, public_key, encrypted_secret, salt) VALUES (0, ?, ?, ?, ?)",
        [
            KEY_TYPE.into(),
            key.key_id().to_vec().into(),
            encrypted.into(),
            salt.to_vec().into(),
        ],
    ))
    .await
    .map_err(|e| format!("Failed to store catalog key: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm::Database;

    use super::*;

    const UUID: &str = "11111111-2222-3333-4444-555555555555";

    async fn db_with_crypto_keys() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "CREATE TABLE crypto_keys (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL,
                key_type TEXT NOT NULL,
                public_key BLOB NOT NULL,
                encrypted_secret BLOB NOT NULL,
                salt BLOB NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                revoked_at TEXT
            )",
        )
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn load_never_creates_and_load_or_create_persists() {
        let db = db_with_crypto_keys().await;
        let store = CatalogKeyStore::new();

        assert!(store.load(&db, UUID).await.unwrap().is_none());

        let created = store.load_or_create(&db, UUID).await.unwrap();
        // A fresh store reads the same key back from the rows.
        let reloaded = CatalogKeyStore::new()
            .load(&db, UUID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.key_id_hex(), created.key_id_hex());
        assert_eq!(*reloaded.export_secret(), *created.export_secret());
    }

    #[tokio::test]
    async fn the_wrong_library_uuid_cannot_open_the_row() {
        let db = db_with_crypto_keys().await;
        CatalogKeyStore::new()
            .load_or_create(&db, UUID)
            .await
            .unwrap();

        let err = CatalogKeyStore::new()
            .load(&db, "other-uuid")
            .await
            .unwrap_err();

        assert!(err.contains("Decryption failed"), "{err}");
    }

    #[tokio::test]
    async fn rotation_retires_the_old_key_and_serves_the_new_one() {
        let db = db_with_crypto_keys().await;
        let store = CatalogKeyStore::new();
        let first = store.load_or_create(&db, UUID).await.unwrap();

        let second = store.rotate(&db, UUID).await.unwrap();

        assert_ne!(first.key_id_hex(), second.key_id_hex());
        assert_eq!(
            store.load(&db, UUID).await.unwrap().unwrap().key_id_hex(),
            second.key_id_hex()
        );
        let reloaded = CatalogKeyStore::new()
            .load(&db, UUID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.key_id_hex(), second.key_id_hex());
        let live: i64 = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT COUNT(*) AS n FROM crypto_keys WHERE key_type = 'catalog' AND revoked_at IS NULL"
                    .to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap();
        assert_eq!(live, 1);
    }
}
