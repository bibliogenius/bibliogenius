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

use crate::crypto::catalog_key::{CATALOG_FORMAT_E2EE_V1, CatalogKey, key_id_of_blob};

use super::hub_directory_service::{HubDirectoryError, HubDirectoryService, HubFollow};

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

    /// Wraps the catalog key for every active follower that published an
    /// X25519 key and hands the wraps to the hub (ADR-072 D3, D4). Returns
    /// how many follow rows the hub updated. Followers without a key are
    /// left as they are: they cannot be sealed for, and the owner's push
    /// decision (D8) already keeps the catalog in clear for them.
    pub async fn wrap_catalog_key_for_followers(
        &self,
        db: &DatabaseConnection,
        key: &CatalogKey,
    ) -> Result<i32, HubDirectoryError> {
        let followers = self.list_followers(db).await?;
        let wraps = plan_catalog_key_wraps(&followers, key)?;
        if wraps.is_empty() {
            return Ok(0);
        }
        self.sync_follow_catalog_keys(db, &wraps).await
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

        let payload: Vec<serde_json::Value> = wraps
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

        #[derive(Deserialize)]
        struct SyncResult {
            updated: i32,
        }
        let result: SyncResult = response
            .json()
            .await
            .map_err(|e| HubDirectoryError::Network(e.to_string()))?;
        Ok(result.updated)
    }
}

/// Wraps `key` for every active follower carrying an X25519 key. A follower
/// whose key is not 32 bytes of hex is skipped, not failed on: one bad
/// profile must not keep every other reader from its wrap.
pub(crate) fn plan_catalog_key_wraps(
    followers: &[HubFollow],
    key: &CatalogKey,
) -> Result<Vec<(i64, Option<String>)>, HubDirectoryError> {
    let mut wraps = Vec::new();
    for follow in followers.iter().filter(|f| f.status == "active") {
        let Some(public_hex) = follow.follower_x25519_public_key.as_deref() else {
            continue;
        };
        let Some(public) = hex::decode(public_hex)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
        else {
            tracing::warn!(
                target: "hub_directory",
                "catalog key wrap skipped for follow {}: malformed x25519 key",
                follow.id
            );
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

    use sea_orm::{ConnectionTrait, Database};
    use serial_test::serial;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};
    use x25519_dalek::PublicKey as X25519PublicKey;

    use super::*;
    use crate::services::hub_directory_service::{
        CatalogEntry, CatalogPushMode, PushCatalogOutcome,
    };

    const OWNER: &str = "owner-node";

    async fn local_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        for sql in [
            "CREATE TABLE hub_directory_config (
                id INTEGER PRIMARY KEY DEFAULT 1,
                node_id TEXT NOT NULL,
                write_token TEXT NOT NULL,
                is_listed INTEGER NOT NULL DEFAULT 0,
                requires_approval INTEGER NOT NULL DEFAULT 1,
                accept_from TEXT NOT NULL DEFAULT 'everyone',
                allow_borrowing INTEGER NOT NULL DEFAULT 1,
                recovery_code TEXT,
                last_catalog_hash TEXT,
                last_catalog_pushed_at TEXT,
                catalog_version INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
            "INSERT INTO hub_directory_config (id, node_id, write_token, created_at, updated_at)
             VALUES (1, 'reader-node', 'reader-token', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            "CREATE TABLE hub_catalog_reader_state (
                node_id TEXT PRIMARY KEY NOT NULL,
                key_id TEXT NOT NULL,
                last_version INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            )",
        ] {
            db.execute_unprepared(sql).await.unwrap();
        }
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

    #[test]
    fn wraps_go_to_active_followers_with_a_key_only() {
        let key = CatalogKey::generate();
        let (_, public) = reader();
        let follows: Vec<HubFollow> = serde_json::from_value(serde_json::json!([
            { "id": 1, "follower_node_id": "a", "followed_node_id": "me", "status": "active",
              "created_at": "x", "follower_x25519_public_key": public },
            { "id": 2, "follower_node_id": "b", "followed_node_id": "me", "status": "pending",
              "created_at": "x", "follower_x25519_public_key": public },
            { "id": 3, "follower_node_id": "c", "followed_node_id": "me", "status": "active",
              "created_at": "x" },
            { "id": 4, "follower_node_id": "d", "followed_node_id": "me", "status": "active",
              "created_at": "x", "follower_x25519_public_key": "not-hex" },
        ]))
        .unwrap();

        let wraps = plan_catalog_key_wraps(&follows, &key).unwrap();

        assert_eq!(wraps.len(), 1);
        assert_eq!(wraps[0].0, 1);
        assert!(wraps[0].1.is_some());
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
}
