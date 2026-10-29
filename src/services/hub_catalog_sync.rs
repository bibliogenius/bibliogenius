//! Pushes the directory catalog to the hub and hands out the catalog key
//! wraps (ADR-072): one orchestration for the FFI entry points and for the
//! two-instance integration tests, instead of a copy per caller.

use sea_orm::DatabaseConnection;

use crate::services::hub_catalog_key_store::CatalogKeyStore;
use crate::services::hub_directory_service::{
    CatalogEntry, CatalogPushMode, HubDirectoryService, PushCatalogOutcome,
};

/// The catalog as the hub receives it, after the cover uploads.
pub struct HubCatalogToPush {
    pub entries: Vec<CatalogEntry>,
    pub book_count: i64,
    /// Ids of the books whose cover the hub now holds (uploaded this run or
    /// replayed from the upload cache): what an encrypted push lists in
    /// clear for the hub's cover GC (ADR-072 D6).
    pub cover_book_ids: Vec<String>,
}

/// What the hub stores: the clear payloads, or a ciphertext under the
/// library's catalog key. The caller decides (ADR-072 D8: encrypt only
/// when every current reader runs a build that can decrypt); the sync
/// never falls back on its own.
pub enum CatalogSyncMode<'a> {
    Clear,
    Encrypted {
        /// At-rest store of the catalog key.
        store: &'a CatalogKeyStore,
        /// The library uuid the identity was initialized with: the key is
        /// stored under the same Argon2 protection as the identity keys.
        library_uuid: &'a str,
    },
}

impl HubDirectoryService {
    /// Pushes a built catalog and, under the catalog key, hands out the
    /// wraps. Always pushes: even with an empty catalog, `book_count` must
    /// reach the hub; `push_catalog` short-circuits when the catalog has not
    /// changed (ADR-027) and the outcome says so, while the entry count is
    /// returned either way so the app clears its dirty flag.
    ///
    /// Encrypted: the key is created on first use, and rotated first when a
    /// reader left since it was made (D4). Covers are uploaded exactly as
    /// for a clear push (D5), and their book ids travel in clear so the hub
    /// can garbage collect them. Once the hub confirms it holds the
    /// ciphertext (stored, or 304 on an unchanged digest), every reader on
    /// the owner's list receives its wrap of the key toward its pinned key,
    /// and any other active follower loses the wrap it held (D3); a locally
    /// skipped push only wraps for readers let in since the last batch.
    /// Fails with a `catalog key wraps:` prefix when the ciphertext went out
    /// but the wraps did not: the next sync then contacts the hub again and
    /// wraps, and the diagnostic beacon can tell the two failures apart.
    pub async fn sync_catalog(
        &self,
        db: &DatabaseConnection,
        catalog: &HubCatalogToPush,
        mode: CatalogSyncMode<'_>,
    ) -> Result<(i32, PushCatalogOutcome), String> {
        let (store, library_uuid) = match mode {
            CatalogSyncMode::Clear => {
                return self.push_and_log(db, catalog, CatalogPushMode::Clear).await;
            }
            CatalogSyncMode::Encrypted {
                store,
                library_uuid,
            } => (store, library_uuid),
        };
        let key = self
            .catalog_key_for_encrypted_push(db, store, library_uuid)
            .await
            .map_err(|e| e.to_string())?;
        let (count, outcome) = self
            .push_and_log(
                db,
                catalog,
                CatalogPushMode::Encrypted {
                    key: &key,
                    cover_book_ids: catalog.cover_book_ids.clone(),
                },
            )
            .await?;
        self.wrap_catalog_key_after_push(db, &key, outcome)
            .await
            .map_err(|e| format!("catalog key wraps: {e}"))?;
        Ok((count, outcome))
    }

    /// The push itself, with the outcome line the acceptance recipe reads
    /// (`hub catalog sync outcome`).
    async fn push_and_log(
        &self,
        db: &DatabaseConnection,
        catalog: &HubCatalogToPush,
        mode: CatalogPushMode<'_>,
    ) -> Result<(i32, PushCatalogOutcome), String> {
        let encrypted = matches!(mode, CatalogPushMode::Encrypted { .. });
        let outcome = self
            .push_catalog(db, &catalog.entries, catalog.book_count, mode)
            .await
            .map_err(|e| e.to_string())?;
        let count = catalog.entries.len() as i32;
        tracing::info!(
            target: "hub_directory",
            outcome = ?outcome,
            count = count,
            encrypted = encrypted,
            "hub catalog sync outcome"
        );
        Ok((count, outcome))
    }
}

#[cfg(test)]
mod tests {
    //! The sync orchestration against a mock hub. Tests are serial because
    //! `HUB_URL` is process-wide.

    use sea_orm::{ConnectionTrait, DatabaseConnection};
    use serial_test::serial;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::services::hub_catalog_key_store::CatalogKeyStore;
    use crate::services::hub_directory_service::{
        CatalogEntry, HubDirectoryService, PushCatalogOutcome,
    };

    const LIBRARY_UUID: &str = "11111111-2222-3333-4444-555555555555";

    async fn registered_db() -> DatabaseConnection {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "INSERT INTO hub_directory_config (id, node_id, write_token, created_at, updated_at)
             VALUES (1, 'owner-node', 'owner-token', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .await
        .unwrap();
        db
    }

    fn point_hub_at(server: &MockServer) {
        // Process-wide, hence #[serial] on every test here.
        unsafe { std::env::set_var("HUB_URL", server.uri()) };
    }

    fn entry(isbn: &str, title: &str) -> CatalogEntry {
        CatalogEntry {
            isbn: isbn.to_string(),
            book_id: Some(format!("uuid-{isbn}")),
            title: title.to_string(),
            author: None,
            cover_url: None,
            added_at: None,
        }
    }

    fn catalog() -> HubCatalogToPush {
        HubCatalogToPush {
            entries: vec![entry("9781", "A"), entry("9782", "B")],
            book_count: 2,
            cover_book_ids: vec![],
        }
    }

    async fn mount_push_accepted(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/api/directory/catalog"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "updated_at": "2026-10-19T10:00:00+00:00",
                "expires_at": "2026-10-26T10:00:00+00:00",
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    fn followers_mock(status: u16) -> Mock {
        Mock::given(method("GET"))
            .and(path("/api/directory/follows"))
            .and(query_param("direction", "followers"))
            .respond_with(
                ResponseTemplate::new(status).set_body_json(serde_json::json!({ "items": [] })),
            )
    }

    /// The `catalog_payload` of each catalog push the hub received: the
    /// entries as a JSON string in clear, a `BGC1` ciphertext otherwise.
    async fn pushed_payloads(server: &MockServer) -> Vec<String> {
        let requests: Vec<Request> = server.received_requests().await.unwrap();
        requests
            .into_iter()
            .filter(|r| r.method == "POST" && r.url.path() == "/api/directory/catalog")
            .map(|r| {
                let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                body["catalog_payload"].as_str().unwrap().to_string()
            })
            .collect()
    }

    #[tokio::test]
    #[serial]
    async fn a_clear_push_reports_its_outcome_and_makes_no_key() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = registered_db().await;
        mount_push_accepted(&server).await;

        let (count, outcome) = HubDirectoryService::new()
            .sync_catalog(&db, &catalog(), CatalogSyncMode::Clear)
            .await
            .unwrap();

        assert_eq!((count, outcome), (2, PushCatalogOutcome::Pushed));
        let payloads = pushed_payloads(&server).await;
        assert!(
            payloads[0].contains("\"title\":\"A\""),
            "a clear push carries the titles: {}",
            payloads[0]
        );
        assert!(
            CatalogKeyStore::new()
                .load(&db, LIBRARY_UUID)
                .await
                .unwrap()
                .is_none(),
            "a clear push creates no catalog key"
        );
    }

    #[tokio::test]
    #[serial]
    async fn an_encrypted_push_makes_the_key_and_sends_a_ciphertext() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = registered_db().await;
        followers_mock(200).mount(&server).await;
        mount_push_accepted(&server).await;
        let store = CatalogKeyStore::new();

        let (count, outcome) = HubDirectoryService::new()
            .sync_catalog(
                &db,
                &catalog(),
                CatalogSyncMode::Encrypted {
                    store: &store,
                    library_uuid: LIBRARY_UUID,
                },
            )
            .await
            .unwrap();

        assert_eq!((count, outcome), (2, PushCatalogOutcome::Pushed));
        assert!(
            store.load(&db, LIBRARY_UUID).await.unwrap().is_some(),
            "the first encrypted push creates the catalog key"
        );
        let payloads = pushed_payloads(&server).await;
        // "BGC1" in base64, the head the recipe's C1 control reads.
        assert!(payloads[0].starts_with("QkdDM"), "{}", payloads[0]);
        assert!(!payloads[0].contains("title"), "a title leaked in clear");
    }

    #[tokio::test]
    #[serial]
    async fn undelivered_wraps_are_reported_as_such_and_reset_the_push_state() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = registered_db().await;
        // The followers are readable when the key is chosen, unreadable
        // when the wraps are handed out after the push.
        followers_mock(200).up_to_n_times(1).mount(&server).await;
        followers_mock(500).mount(&server).await;
        mount_push_accepted(&server).await;
        let store = CatalogKeyStore::new();

        let err = HubDirectoryService::new()
            .sync_catalog(
                &db,
                &catalog(),
                CatalogSyncMode::Encrypted {
                    store: &store,
                    library_uuid: LIBRARY_UUID,
                },
            )
            .await
            .unwrap_err();

        assert!(err.starts_with("catalog key wraps:"), "{err}");
        let cfg = HubDirectoryService::get_config(&db).await.unwrap().unwrap();
        assert_eq!(
            cfg.last_catalog_hash, None,
            "the next sync must contact the hub again"
        );
    }
}
