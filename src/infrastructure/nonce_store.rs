//! SQLite-backed NonceStore for anti-replay protection (B4).
//!
//! Uses the `seen_envelopes` table created by migration 037.
//! Probabilistic cleanup (~1% chance) on each insert to avoid unbounded growth.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::crypto::errors::CryptoError;
use crate::services::crypto_service::NonceStore;

/// Production NonceStore backed by SQLite `seen_envelopes` table.
#[derive(Clone)]
pub struct SqliteNonceStore {
    db: DatabaseConnection,
}

impl SqliteNonceStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Delete envelopes older than 7 days.
    fn cleanup(&self) -> Result<(), CryptoError> {
        // Use tokio::task::block_in_place since NonceStore trait is sync
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.db
                    .execute(Statement::from_string(
                        sea_orm::DatabaseBackend::Sqlite,
                        "DELETE FROM seen_envelopes WHERE received_at < datetime('now', '-7 days')"
                            .to_string(),
                    ))
                    .await
                    .map_err(|e| CryptoError::Serialization(format!("cleanup failed: {e}")))?;
                Ok(())
            })
        })
    }
}

impl NonceStore for SqliteNonceStore {
    fn exists(&self, nonce: &[u8; 12]) -> Result<bool, CryptoError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let result = self
                    .db
                    .query_one(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Sqlite,
                        "SELECT 1 FROM seen_envelopes WHERE nonce = ?",
                        vec![nonce.to_vec().into()],
                    ))
                    .await
                    .map_err(|e| CryptoError::Serialization(format!("nonce check failed: {e}")))?;
                Ok(result.is_some())
            })
        })
    }

    fn insert(&self, nonce: &[u8; 12]) -> Result<(), CryptoError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.db
                    .execute(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Sqlite,
                        "INSERT INTO seen_envelopes (nonce, received_at) VALUES (?, datetime('now'))",
                        vec![nonce.to_vec().into()],
                    ))
                    .await
                    .map_err(|e| CryptoError::Serialization(format!("nonce insert failed: {e}")))?;
                Ok(())
            })
        })?;

        // Probabilistic cleanup: ~1% chance on each insert
        if rand::random::<u8>() < 3 {
            let _ = self.cleanup();
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;

    async fn setup_test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let _ = run_migrations(&db).await;
        db
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_and_exists() {
        let db = setup_test_db().await;
        let store = SqliteNonceStore::new(db);

        let nonce: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

        assert!(!store.exists(&nonce).unwrap());
        store.insert(&nonce).unwrap();
        assert!(store.exists(&nonce).unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_insert_fails() {
        let db = setup_test_db().await;
        let store = SqliteNonceStore::new(db);

        let nonce: [u8; 12] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];

        store.insert(&nonce).unwrap();
        // Second insert should fail (PRIMARY KEY constraint)
        let result = store.insert(&nonce);
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cleanup_removes_old_entries() {
        let db = setup_test_db().await;
        let store = SqliteNonceStore::new(db.clone());

        // Insert a nonce with old timestamp
        let old_nonce: [u8; 12] = [1; 12];
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "INSERT INTO seen_envelopes (nonce, received_at) VALUES (?, datetime('now', '-8 days'))",
            vec![old_nonce.to_vec().into()],
        ))
        .await
        .unwrap();

        // Insert a recent nonce
        let recent_nonce: [u8; 12] = [2; 12];
        store.insert(&recent_nonce).unwrap();

        // Run cleanup
        store.cleanup().unwrap();

        // Old nonce should be gone, recent should remain
        assert!(!store.exists(&old_nonce).unwrap());
        assert!(store.exists(&recent_nonce).unwrap());
    }
}
