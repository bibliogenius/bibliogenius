//! Encrypted directory catalog, service side (ADR-072).
//!
//! What the owner sends the hub for an encrypted push, what a reader does
//! with the ciphertext it gets back, and how the owner's catalog key reaches
//! each reader (wrapped on the follow row, next to the sealed contact card).
//! The crypto itself lives in [`crate::crypto::catalog_key`]; this module
//! wires it to the hub API and to the local state that makes rollback
//! detection possible.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::Deserialize;
use x25519_dalek::StaticSecret;

use std::sync::Arc;

use crate::crypto::catalog_key::{CATALOG_FORMAT_E2EE_V1, CatalogKey, key_id_of_blob};

use super::hub_catalog_key_store::CatalogKeyStore;
use super::hub_catalog_readers::{self, CatalogReaders, ReaderStatus};

use super::hub_directory_service::{
    HubDirectoryError, HubDirectoryService, HubFollow, PushCatalogOutcome,
};

impl HubDirectoryService {
    /// The push body of an encrypted catalog (ADR-072 D6): the ciphertext
    /// as `catalog_payload`, an empty ISBN list, the keyed digest, the book
    /// count in clear, and the ids of the books whose covers the hub holds
    /// so its orphan cover GC can run without reading the catalog.
    pub(crate) fn encrypted_push_body(
        key: &CatalogKey,
        node_id: &str,
        version: i64,
        catalog_payload: &str,
        book_count: i64,
        catalog_hash: &str,
        cover_book_ids: &[String],
    ) -> Result<serde_json::Value, HubDirectoryError> {
        let version = u64::try_from(version)
            .map_err(|_| HubDirectoryError::Config("negative catalog version".into()))?;
        let ciphertext = key
            .encrypt(node_id, version, catalog_payload.as_bytes())
            .map_err(|e| HubDirectoryError::Config(format!("catalog encryption: {e}")))?;
        Ok(serde_json::json!({
            "isbn_payload": "[]",
            "catalog_payload": ciphertext,
            "catalog_format": CATALOG_FORMAT_E2EE_V1,
            "book_count": book_count,
            "catalog_hash": catalog_hash,
            "cover_book_ids": cover_book_ids,
        }))
    }

    /// Records the version the hub now serves, after a 2xx on an encrypted
    /// push, so the next push goes strictly above it.
    pub(crate) async fn record_catalog_version(
        db: &DatabaseConnection,
        version: i64,
    ) -> Result<(), HubDirectoryError> {
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE hub_directory_config SET catalog_version = ?, updated_at = ? WHERE id = 1",
            [version.into(), chrono::Utc::now().to_rfc3339().into()],
        ))
        .await?;
        Ok(())
    }

    /// Opens the ciphertext of `node_id`'s catalog (ADR-072 D7): finds this
    /// library's wrap in its following list, unwraps it with the local X25519
    /// secret, checks the key id and the associated data, refuses a version
    /// below the last one seen, then remembers the version.
    ///
    /// Errors map to the two reader-side codes: `CatalogKeyUnavailable` when
    /// no usable wrap exists (none, unreadable, or for a rotated key), and
    /// `CatalogInvalid` when the ciphertext itself cannot be trusted.
    pub(crate) async fn open_encrypted_catalog(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
        payload: &str,
        reader_x25519_secret: Option<&StaticSecret>,
    ) -> Result<String, HubDirectoryError> {
        let secret = reader_x25519_secret
            .ok_or_else(|| HubDirectoryError::CatalogKeyUnavailable("no local identity".into()))?;

        let follows = self.list_following(db).await?;
        let wrap = follows
            .iter()
            .find(|f| f.followed_node_id == node_id)
            .and_then(|f| f.encrypted_catalog_key.as_deref())
            .filter(|w| !w.is_empty())
            .ok_or_else(|| {
                HubDirectoryError::CatalogKeyUnavailable("no wrap on the follow".into())
            })?;

        let key = CatalogKey::unwrap(secret, wrap).map_err(|e| {
            HubDirectoryError::CatalogKeyUnavailable(format!("wrap unreadable: {e}"))
        })?;
        let blob_key_id = key_id_of_blob(payload)
            .map_err(|e| HubDirectoryError::CatalogInvalid(e.to_string()))?;
        if blob_key_id != key.key_id_hex() {
            return Err(HubDirectoryError::CatalogKeyUnavailable(
                "wrap is for a rotated key".into(),
            ));
        }

        let (version, plaintext) = key
            .decrypt(node_id, payload)
            .map_err(|e| HubDirectoryError::CatalogInvalid(e.to_string()))?;

        let last_seen = Self::last_catalog_version_seen(db, node_id, &blob_key_id).await?;
        if version < last_seen {
            return Err(HubDirectoryError::CatalogInvalid(format!(
                "version {version} below the last seen {last_seen}"
            )));
        }
        Self::record_catalog_version_seen(db, node_id, &blob_key_id, version).await?;

        String::from_utf8(plaintext)
            .map_err(|e| HubDirectoryError::CatalogInvalid(format!("not utf-8: {e}")))
    }

    /// Highest catalog version this reader has accepted from `node_id`
    /// under `key_id`, 0 when none yet. Scoped to the key on purpose: a
    /// rotation or a reinstall starts a new history under a new key id,
    /// and a replay of an older ciphertext under a retired key already
    /// fails on the key id, so only versions under one key compare.
    pub(crate) async fn last_catalog_version_seen(
        db: &DatabaseConnection,
        node_id: &str,
        key_id: &str,
    ) -> Result<u64, HubDirectoryError> {
        let row = db
            .query_one(Statement::from_sql_and_values(
                db.get_database_backend(),
                "SELECT last_version FROM hub_catalog_reader_state WHERE node_id = ? AND key_id = ?",
                [node_id.into(), key_id.into()],
            ))
            .await?;
        Ok(row
            .and_then(|r| r.try_get::<i64>("", "last_version").ok())
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0))
    }

    async fn record_catalog_version_seen(
        db: &DatabaseConnection,
        node_id: &str,
        key_id: &str,
        version: u64,
    ) -> Result<(), HubDirectoryError> {
        let version = i64::try_from(version)
            .map_err(|_| HubDirectoryError::CatalogInvalid("version out of range".into()))?;
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO hub_catalog_reader_state (node_id, key_id, last_version, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(node_id) DO UPDATE SET
                 key_id = excluded.key_id,
                 last_version = excluded.last_version,
                 updated_at = excluded.updated_at",
            [
                node_id.into(),
                key_id.into(),
                version.into(),
                chrono::Utc::now().to_rfc3339().into(),
            ],
        ))
        .await?;
        Ok(())
    }

    /// What follows an encrypted push (ADR-072 D3): once the hub confirmed
    /// it holds this ciphertext, either by storing it (`Pushed`) or by
    /// answering 304 to it (`SkippedRemote`), every active follower gets its
    /// wrap of the key that opens it. A local skip never reached the hub, so
    /// the wraps already there still open the stored ciphertext and nothing
    /// is sent. The 304 case matters: when the wraps cannot be delivered
    /// after a real push, the local push state is reset so the next sync
    /// contacts the hub again, and the hub, holding that same digest,
    /// answers 304. Wrapping on that answer too is what gets the wraps out
    /// without waiting for the next catalog change; it also costs one small
    /// batch on each keep-alive push, every few days. Returns how many
    /// follow rows the hub updated.
    pub async fn wrap_catalog_key_after_push(
        &self,
        db: &DatabaseConnection,
        key: &CatalogKey,
        outcome: PushCatalogOutcome,
    ) -> Result<i32, HubDirectoryError> {
        // A local skip never reached the hub, so the wraps already there
        // still open the stored ciphertext; only a reader let in since the
        // last batch is missing one.
        if outcome == PushCatalogOutcome::SkippedLocal
            && hub_catalog_readers::wraps_due(db)
                .await
                .map_err(HubDirectoryError::Config)?
                .is_none()
        {
            return Ok(0);
        }
        match self.wrap_catalog_key_for_followers(db, key).await {
            Ok(updated) => Ok(updated),
            Err(e) => {
                tracing::warn!(
                    target: "hub_directory",
                    "catalog key wraps not delivered after push, next sync will push again: {e}"
                );
                Self::record_catalog_push_state(db, None).await?;
                Err(e)
            }
        }
    }

    /// Wraps the catalog key for every active follower on the readers list
    /// whose key still matches the pinned one, withdraws it from every other
    /// active follower, and hands the batch to the hub (ADR-072 D3, D4).
    /// Returns how many follow rows the hub updated.
    pub async fn wrap_catalog_key_for_followers(
        &self,
        db: &DatabaseConnection,
        key: &CatalogKey,
    ) -> Result<i32, HubDirectoryError> {
        let listed_at = hub_catalog_readers::now_rfc3339();
        let followers = self.list_followers(db).await?;
        hub_catalog_readers::reconcile(db, &followers, &listed_at)
            .await
            .map_err(HubDirectoryError::Config)?;
        let due = hub_catalog_readers::wraps_due(db)
            .await
            .map_err(HubDirectoryError::Config)?;
        let readers = CatalogReaders::load(db)
            .await
            .map_err(HubDirectoryError::Config)?;
        let wraps = plan_catalog_key_wraps(&followers, &readers, key)?;
        let updated = if wraps.is_empty() {
            0
        } else {
            self.sync_follow_catalog_keys(db, &wraps).await?
        };
        if let Some(through) = due {
            hub_catalog_readers::mark_wrapped(db, through)
                .await
                .map_err(HubDirectoryError::Config)?;
        }
        Ok(updated)
    }

    /// The key the next encrypted push goes out under (ADR-072 D4). Reads
    /// the followers first, so a reader that left is noticed, then rotates
    /// when a departure is newer than the current key, wherever and
    /// whenever it was recorded. Fails, creating and rotating nothing, when
    /// the followers cannot be read: the push is retried later rather than
    /// sent under a key an ex-reader may still hold.
    pub async fn catalog_key_for_encrypted_push(
        &self,
        db: &DatabaseConnection,
        store: &CatalogKeyStore,
        library_uuid: &str,
    ) -> Result<Arc<CatalogKey>, HubDirectoryError> {
        let listed_at = hub_catalog_readers::now_rfc3339();
        let followers = self.list_followers(db).await?;
        hub_catalog_readers::reconcile(db, &followers, &listed_at)
            .await
            .map_err(HubDirectoryError::Config)?;
        let due = hub_catalog_readers::rotation_due(db)
            .await
            .map_err(HubDirectoryError::Config)?;

        let current = store
            .load(db, library_uuid)
            .await
            .map_err(HubDirectoryError::Config)?;
        let key = match (current, due) {
            (Some(key), None) => return Ok(key),
            (Some(_), Some(_)) => {
                let key = store
                    .rotate(db, library_uuid)
                    .await
                    .map_err(HubDirectoryError::Config)?;
                tracing::info!(
                    target: "hub_directory",
                    "catalog key rotated after a reader left"
                );
                key
            }
            // A key created now postdates every departure already counted.
            (None, _) => store
                .load_or_create(db, library_uuid)
                .await
                .map_err(HubDirectoryError::Config)?,
        };
        if let Some(through) = due {
            hub_catalog_readers::mark_rotated(db, through)
                .await
                .map_err(HubDirectoryError::Config)?;
        }
        Ok(key)
    }

    /// Withdraws, right away, the wrap held by `node_id`'s follow when it
    /// still follows this library (ADR-072 D4). The rotation that follows
    /// protects the next ciphertexts; this closes the window before it.
    /// Returns whether a follow row was updated.
    pub async fn withdraw_catalog_key_for(
        &self,
        db: &DatabaseConnection,
        node_id: &str,
    ) -> Result<bool, HubDirectoryError> {
        let followers = self.list_followers(db).await?;
        let Some(follow) = followers
            .iter()
            .find(|f| f.status == "active" && f.follower_node_id == node_id)
        else {
            return Ok(false);
        };
        let updated = self
            .sync_follow_catalog_keys(db, &[(follow.id, None)])
            .await?;
        Ok(updated > 0)
    }

    /// Batch-updates the wrapped catalog key on follow rows; `None`
    /// withdraws a wrap (ADR-072 D4). Same hub route as the contact
    /// re-broadcast, so the two blobs share one request shape.
    pub async fn sync_follow_catalog_keys(
        &self,
        db: &DatabaseConnection,
        wraps: &[(i64, Option<String>)],
    ) -> Result<i32, HubDirectoryError> {
        let cfg = Self::get_config(db)
            .await?
            .ok_or(HubDirectoryError::NotRegistered)?;
        let hub_url = Self::hub_base_url()?;

        #[derive(Deserialize)]
        struct SyncResult {
            updated: i32,
        }
        let mut updated = 0;
        // The hub takes at most this many entries per call.
        for batch in wraps.chunks(CONTACT_SYNC_BATCH) {
            let payload: Vec<serde_json::Value> = batch
                .iter()
                .map(|(id, wrap)| {
                    serde_json::json!({
                        "follow_id": id,
                        "encrypted_catalog_key": wrap,
                    })
                })
                .collect();

            let response = self
                .http_client
                .post(format!("{hub_url}/api/directory/contacts/sync"))
                .header("Authorization", format!("Bearer {}", cfg.write_token))
                .json(&serde_json::json!({ "contacts": payload }))
                .send()
                .await?;

            let status = response.status().as_u16();
            if status >= 400 {
                let msg = response.text().await.unwrap_or_default();
                return Err(HubDirectoryError::Hub(status, msg));
            }
            let result: SyncResult = response
                .json()
                .await
                .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
            updated += result.updated;
        }
        Ok(updated)
    }
}

/// Entries per `/contacts/sync` call: the hub refuses more.
const CONTACT_SYNC_BATCH: usize = 500;

/// Plans the wraps of `key` for the active followers: a wrap toward the
/// pinned key for each reader the hub still describes with that key, and a
/// withdrawal (`None`) for every other active follower that still holds a
/// wrap, so a stale one never outlives a departure or a key change. A
/// follower holding nothing, and pending or resolved follows, are not
/// touched.
pub(crate) fn plan_catalog_key_wraps(
    followers: &[HubFollow],
    readers: &CatalogReaders,
    key: &CatalogKey,
) -> Result<Vec<(i64, Option<String>)>, HubDirectoryError> {
    let mut wraps = Vec::new();
    for follow in followers.iter().filter(|f| f.status == "active") {
        let pinned = match readers.status(follow) {
            ReaderStatus::Reader => readers.pinned_key(&follow.follower_node_id),
            ReaderStatus::KeyMismatch => {
                tracing::warn!(
                    target: "hub_directory",
                    "catalog key withheld from follow {}: the hub returns another key than the one pinned",
                    follow.id
                );
                None
            }
            ReaderStatus::NotReader | ReaderStatus::Unknown => None,
        };
        let Some(public) = pinned
            .and_then(|k| hex::decode(k).ok())
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
        else {
            if follow
                .encrypted_catalog_key
                .as_deref()
                .is_some_and(|w| !w.is_empty())
            {
                wraps.push((follow.id, None));
            }
            continue;
        };
        let wrap = key
            .wrap_for(&public)
            .map_err(|e| HubDirectoryError::Config(format!("catalog key wrap: {e}")))?;
        wraps.push((follow.id, Some(wrap)));
    }
    Ok(wraps)
}

#[cfg(test)]
mod tests {
    //! The hub is a wiremock server reached through `HUB_URL`, the local
    //! state an in-memory SQLite with the two tables the push and read
    //! paths touch. Tests are serial because `HUB_URL` is process-wide.

    use sea_orm::ConnectionTrait;
    use serial_test::serial;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};
    use x25519_dalek::PublicKey as X25519PublicKey;

    use super::*;
    use crate::services::hub_directory_service::{CatalogEntry, CatalogPushMode};

    const OWNER: &str = "owner-node";

    async fn local_db() -> DatabaseConnection {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        db.execute_unprepared(
            "INSERT INTO hub_directory_config (id, node_id, write_token, created_at, updated_at)
             VALUES (1, 'reader-node', 'reader-token', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
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

    fn reader() -> (StaticSecret, String) {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = hex::encode(X25519PublicKey::from(&secret).as_bytes());
        (secret, public)
    }

    fn follow(id: i64, followed: &str, status: &str, wrap: Option<String>) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "follower_node_id": "reader-node",
            "followed_node_id": followed,
            "status": status,
            "created_at": "2026-01-01T00:00:00Z",
            "resolved_at": null,
            "encrypted_contact": null,
            "encrypted_catalog_key": wrap,
        })
    }

    async fn mount_following(server: &MockServer, items: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path("/api/directory/follows"))
            .and(query_param("direction", "following"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": items })),
            )
            .mount(server)
            .await;
    }

    async fn mount_catalog(server: &MockServer, payload: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/directory/{OWNER}/catalog")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "node_id": OWNER,
                "isbn_payload": "[]",
                "catalog_payload": payload,
                "catalog_format": "e2ee-v1",
                "updated_at": "2026-10-19T10:00:00+00:00",
                "expires_at": "2026-10-26T10:00:00+00:00",
            })))
            .mount(server)
            .await;
    }

    // ── owner side: the push ─────────────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn encrypted_push_sends_a_ciphertext_and_records_the_version() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();

        Mock::given(method("POST"))
            .and(path("/api/directory/catalog"))
            .and(header("Authorization", "Bearer reader-token"))
            .and(body_partial_json(serde_json::json!({
                "isbn_payload": "[]",
                "catalog_format": "e2ee-v1",
                "book_count": 2,
                "cover_book_ids": ["uuid-9781"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "updated_at": "2026-10-19T10:00:00+00:00",
                "expires_at": "2026-10-26T10:00:00+00:00",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let outcome = HubDirectoryService::new()
            .push_catalog(
                &db,
                &[entry("9781", "A"), entry("9782", "B")],
                2,
                CatalogPushMode::Encrypted {
                    key: &key,
                    cover_book_ids: vec!["uuid-9781".to_string()],
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, PushCatalogOutcome::Pushed);
        let cfg = HubDirectoryService::get_config(&db).await.unwrap().unwrap();
        assert_eq!(cfg.catalog_version, 1);
        assert_eq!(cfg.last_catalog_hash.map(|h| h.len()), Some(64));

        // The body never carries a title: the payload is a ciphertext the
        // owner's own key opens, nothing else on the wire is readable.
        let requests: Vec<Request> = server.received_requests().await.unwrap();
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(!body.contains("\"A\""), "a title leaked in clear");
        let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
        let (version, plaintext) = key
            .decrypt("reader-node", sent["catalog_payload"].as_str().unwrap())
            .unwrap();
        assert_eq!(version, 1);
        assert!(
            String::from_utf8(plaintext)
                .unwrap()
                .contains("\"title\":\"A\"")
        );
    }

    #[tokio::test]
    #[serial]
    async fn unchanged_encrypted_catalog_is_skipped_locally() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        Mock::given(method("POST"))
            .and(path("/api/directory/catalog"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "updated_at": "2026-10-19T10:00:00+00:00",
                "expires_at": "2026-10-26T10:00:00+00:00",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let svc = HubDirectoryService::new();
        let entries = [entry("9781", "A")];
        let mode = || CatalogPushMode::Encrypted {
            key: &key,
            cover_book_ids: vec![],
        };

        svc.push_catalog(&db, &entries, 1, mode()).await.unwrap();
        let again = svc.push_catalog(&db, &entries, 1, mode()).await.unwrap();

        assert_eq!(again, PushCatalogOutcome::SkippedLocal);
        assert_eq!(
            HubDirectoryService::get_config(&db)
                .await
                .unwrap()
                .unwrap()
                .catalog_version,
            1,
            "a skipped push must not bump the version"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_rotated_key_changes_the_digest_so_the_push_goes_through() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        Mock::given(method("POST"))
            .and(path("/api/directory/catalog"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "updated_at": "2026-10-19T10:00:00+00:00",
                "expires_at": "2026-10-26T10:00:00+00:00",
            })))
            .expect(2)
            .mount(&server)
            .await;
        let svc = HubDirectoryService::new();
        let entries = [entry("9781", "A")];

        let old_key = CatalogKey::generate();
        svc.push_catalog(
            &db,
            &entries,
            1,
            CatalogPushMode::Encrypted {
                key: &old_key,
                cover_book_ids: vec![],
            },
        )
        .await
        .unwrap();
        let new_key = CatalogKey::generate();
        let outcome = svc
            .push_catalog(
                &db,
                &entries,
                1,
                CatalogPushMode::Encrypted {
                    key: &new_key,
                    cover_book_ids: vec![],
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, PushCatalogOutcome::Pushed);
        assert_eq!(
            HubDirectoryService::get_config(&db)
                .await
                .unwrap()
                .unwrap()
                .catalog_version,
            2
        );
    }

    // ── reader side: opening the catalog ─────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn a_reader_with_a_wrap_reads_the_catalog_and_remembers_the_version() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (secret, public) = reader();
        let plaintext = serde_json::to_string(&[entry("9781", "Hidden title")]).unwrap();
        let blob = key.encrypt(OWNER, 5, plaintext.as_bytes()).unwrap();
        let wrap = key
            .wrap_for(&hex::decode(public).unwrap().try_into().unwrap())
            .unwrap();
        mount_catalog(&server, &blob).await;
        mount_following(&server, vec![follow(1, OWNER, "active", Some(wrap))]).await;

        let entries = HubDirectoryService::new()
            .get_catalog(&db, OWNER, Some(&secret))
            .await
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Hidden title");
        assert_eq!(
            HubDirectoryService::last_catalog_version_seen(&db, OWNER, &key.key_id_hex())
                .await
                .unwrap(),
            5
        );
    }

    #[tokio::test]
    #[serial]
    async fn without_a_wrap_the_key_is_unavailable() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (secret, _) = reader();
        mount_catalog(&server, &key.encrypt(OWNER, 1, b"[]").unwrap()).await;
        mount_following(&server, vec![follow(1, OWNER, "pending", None)]).await;

        let err = HubDirectoryService::new()
            .get_catalog(&db, OWNER, Some(&secret))
            .await
            .unwrap_err();

        assert!(
            matches!(err, HubDirectoryError::CatalogKeyUnavailable(_)),
            "{err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_wrap_for_a_rotated_key_is_unavailable_not_invalid() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let (secret, public) = reader();
        let public: [u8; 32] = hex::decode(public).unwrap().try_into().unwrap();
        let old_key = CatalogKey::generate();
        let new_key = CatalogKey::generate();
        mount_catalog(&server, &new_key.encrypt(OWNER, 2, b"[]").unwrap()).await;
        mount_following(
            &server,
            vec![follow(
                1,
                OWNER,
                "active",
                Some(old_key.wrap_for(&public).unwrap()),
            )],
        )
        .await;

        let err = HubDirectoryService::new()
            .get_catalog(&db, OWNER, Some(&secret))
            .await
            .unwrap_err();

        assert!(
            matches!(err, HubDirectoryError::CatalogKeyUnavailable(_)),
            "{err}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_ciphertext_for_another_library_is_invalid() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (secret, public) = reader();
        let public: [u8; 32] = hex::decode(public).unwrap().try_into().unwrap();
        // Encrypted for another node id, served under OWNER by a hub that
        // moved it: the associated data refuses it.
        mount_catalog(&server, &key.encrypt("someone-else", 1, b"[]").unwrap()).await;
        mount_following(
            &server,
            vec![follow(
                1,
                OWNER,
                "active",
                Some(key.wrap_for(&public).unwrap()),
            )],
        )
        .await;

        let err = HubDirectoryService::new()
            .get_catalog(&db, OWNER, Some(&secret))
            .await
            .unwrap_err();

        assert!(matches!(err, HubDirectoryError::CatalogInvalid(_)), "{err}");
    }

    #[tokio::test]
    #[serial]
    async fn a_new_key_starts_a_new_version_baseline() {
        // A reinstalled owner starts again at version 1 under a fresh key.
        // The rollback guard is scoped to one key: a lower version under
        // another key id is a new history, not a replay.
        let db = local_db().await;
        let (secret, public) = reader();
        let public: [u8; 32] = hex::decode(public).unwrap().try_into().unwrap();
        let svc = HubDirectoryService::new();

        let old_key = CatalogKey::generate();
        let old_hub = MockServer::start().await;
        point_hub_at(&old_hub);
        mount_catalog(&old_hub, &old_key.encrypt(OWNER, 9, b"[]").unwrap()).await;
        mount_following(
            &old_hub,
            vec![follow(
                1,
                OWNER,
                "active",
                Some(old_key.wrap_for(&public).unwrap()),
            )],
        )
        .await;
        svc.get_catalog(&db, OWNER, Some(&secret)).await.unwrap();

        let new_key = CatalogKey::generate();
        let new_hub = MockServer::start().await;
        point_hub_at(&new_hub);
        mount_catalog(&new_hub, &new_key.encrypt(OWNER, 1, b"[]").unwrap()).await;
        mount_following(
            &new_hub,
            vec![follow(
                1,
                OWNER,
                "active",
                Some(new_key.wrap_for(&public).unwrap()),
            )],
        )
        .await;

        svc.get_catalog(&db, OWNER, Some(&secret)).await.unwrap();
        assert_eq!(
            HubDirectoryService::last_catalog_version_seen(&db, OWNER, &new_key.key_id_hex())
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_version_below_the_last_seen_is_refused() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (secret, public) = reader();
        let public: [u8; 32] = hex::decode(public).unwrap().try_into().unwrap();
        mount_following(
            &server,
            vec![follow(
                1,
                OWNER,
                "active",
                Some(key.wrap_for(&public).unwrap()),
            )],
        )
        .await;
        let svc = HubDirectoryService::new();

        // Version 9 accepted first (its own mock, then replaced).
        let v9 = MockServer::start().await;
        point_hub_at(&v9);
        mount_catalog(&v9, &key.encrypt(OWNER, 9, b"[]").unwrap()).await;
        mount_following(
            &v9,
            vec![follow(
                1,
                OWNER,
                "active",
                Some(key.wrap_for(&public).unwrap()),
            )],
        )
        .await;
        svc.get_catalog(&db, OWNER, Some(&secret)).await.unwrap();

        // Then the hub serves an older ciphertext: rollback.
        point_hub_at(&server);
        mount_catalog(&server, &key.encrypt(OWNER, 4, b"[]").unwrap()).await;
        let err = svc
            .get_catalog(&db, OWNER, Some(&secret))
            .await
            .unwrap_err();

        assert!(matches!(err, HubDirectoryError::CatalogInvalid(_)), "{err}");
        assert_eq!(
            HubDirectoryService::last_catalog_version_seen(&db, OWNER, &key.key_id_hex())
                .await
                .unwrap(),
            9
        );
    }

    #[tokio::test]
    #[serial]
    async fn without_a_local_identity_the_key_is_unavailable() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        mount_catalog(&server, &key.encrypt(OWNER, 1, b"[]").unwrap()).await;

        let err = HubDirectoryService::new()
            .get_catalog(&db, OWNER, None)
            .await
            .unwrap_err();

        assert!(
            matches!(err, HubDirectoryError::CatalogKeyUnavailable(_)),
            "{err}"
        );
    }

    // ── owner side: wraps for the readers ────────────────────────────────

    #[tokio::test]
    async fn wraps_go_to_readers_only_and_every_other_active_follower_loses_its_own() {
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (secret, public) = reader();
        let (_, other) = reader();
        hub_catalog_readers::enrol(
            &db,
            "a",
            &public,
            hub_catalog_readers::ReaderSource::Approval,
        )
        .await
        .unwrap();
        hub_catalog_readers::enrol(
            &db,
            "e",
            &public,
            hub_catalog_readers::ReaderSource::Approval,
        )
        .await
        .unwrap();
        let readers = CatalogReaders::load(&db).await.unwrap();
        let follows: Vec<HubFollow> = serde_json::from_value(serde_json::json!([
            { "id": 1, "follower_node_id": "a", "followed_node_id": "me", "status": "active",
              "created_at": "x", "follower_x25519_public_key": public },
            { "id": 2, "follower_node_id": "b", "followed_node_id": "me", "status": "pending",
              "created_at": "x", "follower_x25519_public_key": public },
            { "id": 3, "follower_node_id": "c", "followed_node_id": "me", "status": "active",
              "created_at": "x", "encrypted_catalog_key": "old-wrap" },
            { "id": 4, "follower_node_id": "d", "followed_node_id": "me", "status": "active",
              "created_at": "x", "follower_x25519_public_key": public,
              "encrypted_catalog_key": "old-wrap" },
            // A reader the hub now describes with another key.
            { "id": 5, "follower_node_id": "e", "followed_node_id": "me", "status": "active",
              "created_at": "x", "follower_x25519_public_key": other,
              "encrypted_catalog_key": "old-wrap" },
            // Not a reader and holding nothing: nothing to send it.
            { "id": 6, "follower_node_id": "f", "followed_node_id": "me", "status": "active",
              "created_at": "x", "follower_x25519_public_key": public },
        ]))
        .unwrap();

        let wraps = plan_catalog_key_wraps(&follows, &readers, &key).unwrap();

        let by_id: std::collections::HashMap<i64, Option<String>> = wraps.into_iter().collect();
        assert_eq!(
            by_id.len(),
            4,
            "the pending follow and a follower holding no wrap are not touched"
        );
        let opened = CatalogKey::unwrap(&secret, by_id[&1].as_deref().unwrap()).unwrap();
        assert_eq!(opened.key_id_hex(), key.key_id_hex());
        assert_eq!(by_id[&3], None, "not a reader");
        assert_eq!(by_id[&4], None, "a key is not an invitation");
        assert_eq!(
            by_id[&5], None,
            "never wrapped for a key that was not pinned"
        );
    }

    #[tokio::test]
    #[serial]
    async fn wraps_are_posted_on_the_contact_sync_route() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (secret, public) = reader();
        Mock::given(method("GET"))
            .and(path("/api/directory/follows"))
            .and(query_param("direction", "followers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": [
                { "id": 7, "follower_node_id": "a", "followed_node_id": "reader-node", "status": "active",
                  "created_at": "x", "follower_x25519_public_key": public },
            ]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .and(header("Authorization", "Bearer reader-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 1 })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let updated = HubDirectoryService::new()
            .wrap_catalog_key_for_followers(&db, &key)
            .await
            .unwrap();

        assert_eq!(updated, 1);
        let requests = server.received_requests().await.unwrap();
        let sync = requests
            .iter()
            .find(|r| r.url.path() == "/api/directory/contacts/sync")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sync.body).unwrap();
        assert_eq!(body["contacts"][0]["follow_id"], 7);
        assert!(
            body["contacts"][0].get("encrypted_contact").is_none(),
            "the card is not touched"
        );
        let wrap = body["contacts"][0]["encrypted_catalog_key"]
            .as_str()
            .unwrap();
        let opened = CatalogKey::unwrap(&secret, wrap).unwrap();
        assert_eq!(opened.key_id_hex(), key.key_id_hex());
    }

    async fn mount_one_follower(server: &MockServer, public: &str) {
        Mock::given(method("GET"))
            .and(path("/api/directory/follows"))
            .and(query_param("direction", "followers"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": [
                { "id": 7, "follower_node_id": "a", "followed_node_id": "reader-node", "status": "active",
                  "created_at": "x", "follower_x25519_public_key": public },
            ]})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    #[serial]
    async fn wraps_follow_a_push_the_hub_stored() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (_, public) = reader();
        mount_one_follower(&server, &public).await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 1 })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let updated = HubDirectoryService::new()
            .wrap_catalog_key_after_push(&db, &key, PushCatalogOutcome::Pushed)
            .await
            .unwrap();

        assert_eq!(updated, 1);
    }

    #[tokio::test]
    #[serial]
    async fn a_locally_skipped_push_sends_no_wrap() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (_, public) = reader();
        mount_one_follower(&server, &public).await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let updated = HubDirectoryService::new()
            .wrap_catalog_key_after_push(&db, &key, PushCatalogOutcome::SkippedLocal)
            .await
            .unwrap();

        assert_eq!(updated, 0);
    }

    #[tokio::test]
    #[serial]
    async fn a_locally_skipped_push_still_wraps_for_a_reader_just_let_in() {
        // A peer paired again while its follow stayed active is let back in
        // by the reconciliation, but the catalog did not change: without
        // this, it would wait days for the next real push.
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (_, public) = reader();
        mount_one_follower(&server, &public).await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 1 })),
            )
            .expect(1)
            .mount(&server)
            .await;
        hub_catalog_readers::enrol(&db, "a", &public, hub_catalog_readers::ReaderSource::Paired)
            .await
            .unwrap();
        let svc = HubDirectoryService::new();

        let first = svc
            .wrap_catalog_key_after_push(&db, &key, PushCatalogOutcome::SkippedLocal)
            .await
            .unwrap();
        let second = svc
            .wrap_catalog_key_after_push(&db, &key, PushCatalogOutcome::SkippedLocal)
            .await
            .unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 0, "once delivered, a skip sends nothing again");
    }

    #[tokio::test]
    #[serial]
    async fn after_undelivered_wraps_the_next_sync_is_answered_304_and_still_wraps() {
        // Second round of the failure above, end to end through push_catalog:
        // the hub already holds this digest, so the re-push comes back 304.
        // The wraps must go out on that answer, or the readers would wait
        // for the next catalog change.
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (_, public) = reader();
        mount_one_follower(&server, &public).await;
        Mock::given(method("POST"))
            .and(path("/api/directory/catalog"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 1 })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let svc = HubDirectoryService::new();
        // State left by the failed first round: no local digest.
        HubDirectoryService::record_catalog_push_state(&db, None)
            .await
            .unwrap();

        let outcome = svc
            .push_catalog(
                &db,
                &[entry("9781", "A")],
                1,
                CatalogPushMode::Encrypted {
                    key: &key,
                    cover_book_ids: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, PushCatalogOutcome::SkippedRemote);

        let updated = svc
            .wrap_catalog_key_after_push(&db, &key, outcome)
            .await
            .unwrap();

        assert_eq!(updated, 1);
    }

    #[tokio::test]
    #[serial]
    async fn undelivered_wraps_reset_the_push_state_so_the_next_sync_pushes_again() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let (_, public) = reader();
        mount_one_follower(&server, &public).await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;
        // As if the push had just been confirmed.
        HubDirectoryService::record_catalog_push_state(&db, Some("digest"))
            .await
            .unwrap();

        let err = HubDirectoryService::new()
            .wrap_catalog_key_after_push(&db, &key, PushCatalogOutcome::Pushed)
            .await
            .unwrap_err();

        assert!(matches!(err, HubDirectoryError::Hub(500, _)), "{err}");
        let cfg = HubDirectoryService::get_config(&db).await.unwrap().unwrap();
        assert_eq!(cfg.last_catalog_hash, None, "the digest must be forgotten");
        assert_eq!(cfg.last_catalog_pushed_at, None);
    }

    #[tokio::test]
    #[serial]
    async fn a_null_wrap_withdraws_the_key() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .and(body_partial_json(serde_json::json!({
                "contacts": [{ "follow_id": 3, "encrypted_catalog_key": null }]
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 1 })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let updated = HubDirectoryService::new()
            .sync_follow_catalog_keys(&db, &[(3, None)])
            .await
            .unwrap();

        assert_eq!(updated, 1);
    }

    // ── owner side: readers, departures and rotation ─────────────────────

    async fn mount_followers(server: &MockServer, items: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/api/directory/follows"))
            .and(query_param("direction", "followers"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "items": items })),
            )
            .mount(server)
            .await;
    }

    const LIB: &str = "11111111-2222-3333-4444-555555555555";

    #[tokio::test]
    #[serial]
    async fn an_ex_peer_still_following_never_receives_the_new_key() {
        // The 2026-10-21 recette: the owner removed the contact, the
        // ex-peer's device had not unfollowed yet, and the next rotation
        // wrapped the new key for it because the hub still listed it.
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let (ex_secret, ex_public) = reader();
        let (stay_secret, stay_public) = reader();
        mount_followers(
            &server,
            serde_json::json!([
                { "id": 1, "follower_node_id": "ex-peer", "followed_node_id": "reader-node",
                  "status": "active", "created_at": "x", "follower_x25519_public_key": ex_public,
                  "encrypted_catalog_key": "wrap-of-the-old-key" },
                { "id": 2, "follower_node_id": "stays", "followed_node_id": "reader-node",
                  "status": "active", "created_at": "x", "follower_x25519_public_key": stay_public },
            ]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 2 })),
            )
            .mount(&server)
            .await;
        let svc = HubDirectoryService::new();
        let store = CatalogKeyStore::new();
        let first = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();

        hub_catalog_readers::record_pairing_end(&db, Some("ex-peer"), None)
            .await
            .unwrap();
        let next = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();
        svc.wrap_catalog_key_for_followers(&db, &next)
            .await
            .unwrap();

        assert_ne!(first.key_id_hex(), next.key_id_hex(), "the key rotated");
        let requests = server.received_requests().await.unwrap();
        let sync = requests
            .iter()
            .rev()
            .find(|r| r.url.path() == "/api/directory/contacts/sync")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sync.body).unwrap();
        let entries = body["contacts"].as_array().unwrap();
        let of = |id: i64| {
            entries.iter().find(|e| e["follow_id"] == id).unwrap()["encrypted_catalog_key"].clone()
        };
        assert_eq!(of(1), serde_json::Value::Null, "the ex-peer loses its wrap");
        assert!(CatalogKey::unwrap(&ex_secret, of(2).as_str().unwrap()).is_err());
        let opened = CatalogKey::unwrap(&stay_secret, of(2).as_str().unwrap()).unwrap();
        assert_eq!(opened.key_id_hex(), next.key_id_hex());
    }

    #[tokio::test]
    #[serial]
    async fn a_departure_recorded_by_a_closed_run_still_rotates_the_key() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let (_, public) = reader();
        mount_followers(
            &server,
            serde_json::json!([
                { "id": 1, "follower_node_id": "a", "followed_node_id": "reader-node",
                  "status": "active", "created_at": "x", "follower_x25519_public_key": public },
            ]),
        )
        .await;
        let svc = HubDirectoryService::new();
        let before = svc
            .catalog_key_for_encrypted_push(&db, &CatalogKeyStore::new(), LIB)
            .await
            .unwrap();
        hub_catalog_readers::record_pairing_end(&db, Some("a"), None)
            .await
            .unwrap();

        // A new run: fresh key store, no in-memory state at all.
        let store = CatalogKeyStore::new();
        let after = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();
        let again = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();

        assert_ne!(before.key_id_hex(), after.key_id_hex());
        assert_eq!(
            after.key_id_hex(),
            again.key_id_hex(),
            "one rotation per departure"
        );
    }

    #[tokio::test]
    #[serial]
    async fn without_a_departure_the_key_stays() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        mount_followers(&server, serde_json::json!([])).await;
        let svc = HubDirectoryService::new();
        let store = CatalogKeyStore::new();

        let first = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();
        let second = svc
            .catalog_key_for_encrypted_push(&db, &CatalogKeyStore::new(), LIB)
            .await
            .unwrap();

        assert_eq!(first.key_id_hex(), second.key_id_hex());
    }

    #[tokio::test]
    #[serial]
    async fn a_departure_before_any_key_costs_no_extra_rotation() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let (_, public) = reader();
        hub_catalog_readers::enrol(&db, "a", &public, hub_catalog_readers::ReaderSource::Paired)
            .await
            .unwrap();
        hub_catalog_readers::record_pairing_end(&db, Some("a"), None)
            .await
            .unwrap();
        mount_followers(&server, serde_json::json!([])).await;
        let svc = HubDirectoryService::new();
        let store = CatalogKeyStore::new();

        let created = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();
        let next = svc
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap();

        assert_eq!(created.key_id_hex(), next.key_id_hex());
        assert_eq!(hub_catalog_readers::rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    #[serial]
    async fn an_unreadable_followers_list_creates_and_rotates_nothing() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        Mock::given(method("GET"))
            .and(path("/api/directory/follows"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let store = CatalogKeyStore::new();

        let err = HubDirectoryService::new()
            .catalog_key_for_encrypted_push(&db, &store, LIB)
            .await
            .unwrap_err();

        assert!(matches!(err, HubDirectoryError::Hub(503, _)), "{err}");
        assert!(store.load(&db, LIB).await.unwrap().is_none());
    }

    #[tokio::test]
    #[serial]
    async fn the_wrap_of_a_departed_reader_is_withdrawn_at_once() {
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let (_, public) = reader();
        mount_followers(
            &server,
            serde_json::json!([
                { "id": 9, "follower_node_id": "ex-peer", "followed_node_id": "reader-node",
                  "status": "active", "created_at": "x", "follower_x25519_public_key": public },
            ]),
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .and(body_partial_json(serde_json::json!({
                "contacts": [{ "follow_id": 9, "encrypted_catalog_key": null }]
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": 1 })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let svc = HubDirectoryService::new();

        assert!(svc.withdraw_catalog_key_for(&db, "ex-peer").await.unwrap());
        assert!(
            !svc.withdraw_catalog_key_for(&db, "never-followed")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    #[serial]
    async fn wraps_go_out_in_batches_the_hub_accepts() {
        // The hub refuses more than 500 entries per call.
        let server = MockServer::start().await;
        point_hub_at(&server);
        let db = local_db().await;
        let key = CatalogKey::generate();
        let followers: Vec<serde_json::Value> = (1..=600)
            .map(|i| {
                let (_, public) = reader();
                serde_json::json!({
                    "id": i, "follower_node_id": format!("node-{i}"),
                    "followed_node_id": "reader-node", "status": "active",
                    "created_at": "x", "follower_x25519_public_key": public,
                })
            })
            .collect();
        mount_followers(&server, serde_json::Value::Array(followers)).await;
        Mock::given(method("POST"))
            .and(path("/api/directory/contacts/sync"))
            .respond_with(|req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let n = body["contacts"].as_array().unwrap().len();
                if n > 500 {
                    ResponseTemplate::new(400)
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "updated": n }))
                }
            })
            .mount(&server)
            .await;

        let updated = HubDirectoryService::new()
            .wrap_catalog_key_for_followers(&db, &key)
            .await
            .unwrap();

        assert_eq!(updated, 600);
    }
}
