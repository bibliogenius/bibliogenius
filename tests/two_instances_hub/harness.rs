//! Two backends of this crate on this machine, each with its own database,
//! identity and HTTP server, against the disposable dev hub started with
//! `make it-up` in bibliogenius-hub. Owner-side actions go through the
//! services and the Axum handlers with the instance's own `AppState`, as
//! the FFI layer does for the app, never through the FFI functions
//! themselves (process-wide singletons).

use std::io::Write;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::{delete, post, put};
use rust_lib_app::api;
use rust_lib_app::crypto::envelope::ClearMessage;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{peer, relay_config};
use rust_lib_app::services::IdentityService;
use rust_lib_app::services::hub_catalog_key_store::CatalogKeyStore;
use rust_lib_app::services::hub_catalog_sync::{CatalogSyncMode, HubCatalogToPush};
use rust_lib_app::services::hub_directory_service::{
    CatalogEntry, DirectoryConfig, HubDirectoryService, PushCatalogOutcome, RegisterParams,
};
use rust_lib_app::services::nudge_events::NudgeSource;
use rust_lib_app::services::relay_poller;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tower::ServiceExt;

/// Where `make it-up` publishes the throwaway hub.
pub const DEFAULT_HUB_URL: &str = "http://127.0.0.1:8092";

/// The same filter as the app's debug log file (`api/frb/lifecycle.rs`), so
/// a line asserted here is a line the acceptance recipe can read there.
const LOG_FILTER: &str = "rust_lib_app=info,ssrf=warn,hub_directory=info";

// ── Hub ──────────────────────────────────────────────────────────────────

/// The throwaway hub: its URL for the backends, its compose project for the
/// SQL checks (the same queries as the recipe's C1 and C1 bis controls).
pub struct Hub {
    pub url: String,
    compose: String,
}

impl Hub {
    /// Reads `BG_IT_HUB_URL` and `BG_IT_COMPOSE`, with the `make it-up`
    /// defaults, and points the services at that hub.
    pub fn from_env() -> Self {
        let url = std::env::var("BG_IT_HUB_URL").unwrap_or_else(|_| DEFAULT_HUB_URL.to_string());
        let hub_dir = format!("{}/../bibliogenius-hub", env!("CARGO_MANIFEST_DIR"));
        let compose = std::env::var("BG_IT_COMPOSE").unwrap_or_else(|_| {
            format!(
                "docker compose -p bg-it -f {hub_dir}/docker-compose.dev.yml -f {hub_dir}/docker-compose.it.yml"
            )
        });
        // The services read the hub from the environment, process-wide:
        // every test of this binary is serial for that reason.
        unsafe { std::env::set_var("HUB_URL", &url) };
        Self { url, compose }
    }

    /// Fails with the command to run when no hub answers.
    pub async fn require_ready(&self) {
        let answered = reqwest::Client::new()
            .get(format!("{}/api/health", self.url))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        assert!(
            answered,
            "no hub at {}: run `make it-up` in bibliogenius-hub first, or set BG_IT_HUB_URL",
            self.url
        );
    }

    /// Runs a compose subcommand off the runtime: the backends keep
    /// serving while the process runs.
    async fn compose(&self, args: &str, stdin: Option<&str>) -> String {
        let command = format!("{} {args}", self.compose);
        let input = stdin.map(str::to_string);
        let shown = command.clone();
        tokio::task::spawn_blocking(move || {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(&command)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("docker compose");
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(input.as_bytes())
                    .expect("write stdin");
            }
            let output = child.wait_with_output().expect("compose exit");
            assert!(
                output.status.success(),
                "`{shown}` failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        })
        .await
        .expect("compose task")
    }

    /// Runs SQL on the hub database, unaligned tuples, one row per line.
    pub async fn psql(&self, sql: &str) -> String {
        self.compose(
            "exec -T postgres psql -U hub -d hub_local -At -v ON_ERROR_STOP=1",
            Some(sql),
        )
        .await
        .trim_end()
        .to_string()
    }

    /// What the hub stores for `node_id`'s catalog, as the recipe's C1 and
    /// C1 bis controls read it: format, the catalog key id (hex, from the
    /// `BGC1` blob header, empty in clear) and a short digest of the
    /// payload. The key id is read whole: the recipe's six-character head
    /// only shows four bits of it, which two keys share one time in
    /// sixteen. None when nothing is stored.
    pub async fn catalog_fingerprint(&self, node_id: &str) -> Option<(String, String, String)> {
        let row = self
            .psql(&format!(
                "SELECT coalesce(catalog_format, ''),
                        CASE WHEN catalog_format = 'e2ee-v1'
                             THEN encode(substring(decode(catalog_payload, 'base64') from 5 for 16), 'hex')
                             ELSE '' END,
                        left(md5(catalog_payload), 12)
                   FROM cached_catalogs WHERE node_id = '{node_id}'"
            ))
            .await;
        if row.is_empty() {
            return None;
        }
        let mut parts = row.split('|').map(str::to_string);
        Some((
            parts.next().expect("format"),
            parts.next().expect("key id"),
            parts.next().expect("digest"),
        ))
    }

    /// Whether `node_id`'s stored catalog carries `title` in clear.
    pub async fn catalog_shows(&self, node_id: &str, title: &str) -> bool {
        self.psql(&format!(
            "SELECT count(*) FROM cached_catalogs
              WHERE node_id = '{node_id}' AND catalog_payload ILIKE '%{title}%'"
        ))
        .await
            == "1"
    }

    /// The follow of `follower` toward `followed` on the hub: its status
    /// and a short digest of the catalog key wrap it holds, empty without
    /// a wrap. None when the hub holds no such follow.
    pub async fn follow_state(&self, follower: &str, followed: &str) -> Option<(String, String)> {
        let row = self
            .psql(&format!(
                "SELECT status,
                        CASE WHEN coalesce(encrypted_catalog_key, '') = '' THEN ''
                             ELSE left(md5(encrypted_catalog_key), 12) END
                   FROM follows
                  WHERE follower_node_id = '{follower}' AND followed_node_id = '{followed}'"
            ))
            .await;
        if row.is_empty() {
            return None;
        }
        let (status, digest) = row.split_once('|').expect("status|digest");
        Some((status.to_string(), digest.to_string()))
    }

    /// Empties what a test leaves on the hub, and the per-IP token buckets
    /// of the relay routes: every backend here shares the host's address.
    ///
    /// Refuses to run against anything but the throwaway project: a
    /// `BG_IT_COMPOSE` pointed at the dev stack would empty its database.
    /// `BG_IT_ALLOW_RESET=1` lifts the check for a project named otherwise.
    pub async fn reset(&self) {
        let throwaway = self.compose.contains("-p bg-it")
            || std::env::var("BG_IT_ALLOW_RESET").is_ok_and(|v| v == "1");
        assert!(
            throwaway,
            "refusing to reset a hub outside the bg-it compose project ({}); set BG_IT_ALLOW_RESET=1 if that database is disposable",
            self.compose
        );
        self.psql(
            "TRUNCATE library_profiles, cached_catalogs, follows, relay_mailboxes, relay_messages,
             borrow_requests, invite_tokens, library_view_cooldowns, registration_failures,
             hub_events RESTART IDENTITY CASCADE",
        )
        .await;
        self.compose(
            "exec -T hub php bin/console cache:pool:clear cache.rate_limiter --no-interaction",
            None,
        )
        .await;
    }
}

// ── Captured log ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything the backends log, under the app's own filter.
pub struct Logs(Arc<Mutex<Vec<u8>>>);

static LOGS: OnceLock<Logs> = OnceLock::new();

/// The capture, installed once per test binary.
pub fn logs() -> &'static Logs {
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(buffer.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(LOG_FILTER))
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one capture per binary");
        Logs(buffer)
    })
}

impl Logs {
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    pub fn line_with(&self, needle: &str) -> Option<String> {
        self.text()
            .lines()
            .find(|line| line.contains(needle))
            .map(str::to_string)
    }

    /// The first line containing `needle`, waiting for it up to `timeout`.
    pub async fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let started = Instant::now();
        loop {
            if let Some(line) = self.line_with(needle) {
                return line;
            }
            assert!(
                started.elapsed() < timeout,
                "no log line containing {needle:?} within {timeout:?}; captured:\n{}",
                self.text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

// ── Backend instance ─────────────────────────────────────────────────────

/// One backend: an in-memory database, a fresh identity and the public
/// API served on a loopback port.
pub struct Instance {
    pub name: &'static str,
    pub db: DatabaseConnection,
    pub state: AppState,
    pub svc: HubDirectoryService,
    pub key_store: CatalogKeyStore,
    pub library_uuid: String,
    pub port: u16,
    server: tokio::task::JoinHandle<()>,
}

impl Instance {
    pub async fn start(name: &'static str) -> Self {
        let db = rust_lib_app::db::init_db("sqlite::memory:")
            .await
            .expect("in-memory database");
        let library_uuid = uuid::Uuid::new_v4().to_string();
        let identity = Arc::new(IdentityService::new(db.clone()));
        identity.init(&library_uuid).await.expect("identity");
        let state = AppState::with_identity_service(db.clone(), identity);
        state
            .crypto_service()
            .expect("crypto service built from the identity");

        let router = Router::new().nest("/api", api::api_router_with_state(state.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback port");
        let port = listener.local_addr().expect("local address").port();
        state.set_server_port(port);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .expect("backend server");
        });
        tracing::info!("[{name}] backend listening on 127.0.0.1:{port}");

        Self {
            name,
            db,
            state,
            svc: HubDirectoryService::new(),
            key_store: CatalogKeyStore::new(),
            library_uuid,
            port,
            server,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The hub node id: the library uuid, as the app registers it.
    pub fn node_id(&self) -> &str {
        &self.library_uuid
    }

    /// Calls an owner handler the way the app does over its local HTTP
    /// port, on a router holding this instance's state. Fails on any
    /// non-success status, with the error body.
    async fn owner_call(
        &self,
        app: Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = app.oneshot(request.body(body).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        assert!(
            status.is_success(),
            "[{}] {method} {uri} failed: {status} {json}",
            self.name
        );
        json
    }

    /// Creates a relay mailbox on the hub through the owner handler, the
    /// path "Connect" takes in the app. Returns the mailbox uuid only: the
    /// handler's body also carries the write token, which no test prints.
    pub async fn setup_relay(&self, hub: &Hub) -> String {
        let app = Router::new()
            .route("/relay/setup", post(api::peer::setup_relay))
            .with_state(self.state.clone());
        let json = self
            .owner_call(
                app,
                "POST",
                "/relay/setup",
                Some(serde_json::json!({ "relay_url": hub.url })),
            )
            .await;
        json["mailbox_uuid"]
            .as_str()
            .expect("mailbox uuid in the relay setup answer")
            .to_string()
    }

    /// What this backend answers on `/api/config`, the public identity a
    /// peer reads at pairing time.
    pub async fn config(&self) -> serde_json::Value {
        reqwest::get(format!("{}/api/config", self.url()))
            .await
            .expect("config")
            .json()
            .await
            .expect("config json")
    }

    /// Pairs with `peer` as the app does after a scan on the LAN: the
    /// handshake posted to the peer's `/api/peers/incoming` with our
    /// identity and relay credentials, then the peer stored here through
    /// `connect` with what the handshake answered. Returns our row id of
    /// the peer.
    pub async fn pair_with(&self, peer: &Instance) -> i32 {
        let mine = self.config().await;
        let mut handshake = serde_json::json!({ "name": self.name, "url": self.url() });
        for field in [
            "library_uuid",
            "ed25519_public_key",
            "x25519_public_key",
            "relay_url",
            "mailbox_id",
            "relay_write_token",
        ] {
            if !mine[field].is_null() {
                handshake[field] = mine[field].clone();
            }
        }
        let answer = reqwest::Client::new()
            .post(format!("{}/api/peers/incoming", peer.url()))
            .json(&handshake)
            .send()
            .await
            .expect("handshake");
        assert!(
            answer.status().is_success(),
            "[{}] handshake with {} refused: {}",
            self.name,
            peer.name,
            answer.status()
        );
        let answer: serde_json::Value = answer.json().await.expect("handshake json");
        let mut scan = serde_json::json!({ "name": peer.name, "url": peer.url() });
        for field in [
            "ed25519_public_key",
            "x25519_public_key",
            "relay_url",
            "mailbox_id",
            "relay_write_token",
        ] {
            if !answer[field].is_null() {
                scan[field] = answer[field].clone();
            }
        }
        let app = Router::new()
            .route("/peers/connect", post(api::peer::connect))
            .with_state(self.db.clone());
        let json = self
            .owner_call(app, "POST", "/peers/connect", Some(scan))
            .await;
        json["id"].as_i64().expect("peer id") as i32
    }

    /// Our row of the peer known by `library_uuid`, if any.
    pub async fn peer_row(&self, library_uuid: &str) -> Option<peer::Model> {
        peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(library_uuid))
            .one(&self.db)
            .await
            .expect("peers query")
    }

    /// The catalog readers row of `node_id`: its revocation reason and the
    /// pinned X25519 key, empty when the library never was a reader.
    pub async fn catalog_reader(&self, node_id: &str) -> Option<(Option<String>, String)> {
        use sea_orm::{ConnectionTrait, Statement};
        self.db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT revoke_reason, x25519_public_key FROM catalog_readers WHERE node_id = ?",
                [node_id.into()],
            ))
            .await
            .expect("catalog_readers query")
            .map(|row| {
                (
                    row.try_get("", "revoke_reason").expect("revoke_reason"),
                    row.try_get("", "x25519_public_key")
                        .expect("x25519_public_key"),
                )
            })
    }

    /// Stops answering on the LAN, as a device gone to sleep or in airplane
    /// mode; the database and the identity stay.
    pub fn go_offline(&self) {
        self.server.abort();
    }

    /// Our X25519 public key, hex, as the hub publishes it.
    pub fn x25519_public_hex(&self) -> String {
        self.state
            .identity_service
            .get_public_keys_hex()
            .expect("public keys")
            .1
    }

    /// Asks the hub to follow `owner`'s library, with our key so the owner
    /// can wrap its catalog key for us.
    pub async fn follow(&self, owner: &Instance) {
        self.svc
            .follow(&self.db, owner.node_id(), Some(&self.x25519_public_hex()))
            .await
            .unwrap_or_else(|e| panic!("[{}] follow failed: {e}", self.name));
    }

    /// Stops following `owner`'s library on the hub.
    pub async fn unfollow(&self, owner: &Instance) {
        self.svc
            .unfollow(&self.db, owner.node_id())
            .await
            .unwrap_or_else(|e| panic!("[{}] unfollow failed: {e}", self.name));
    }

    /// Approves the pending follow request of `follower` on the hub. The
    /// reader joins the catalog readers list at the next encrypted push,
    /// through the list's bootstrap: the approval-time admission is an FFI
    /// orchestration the harness does not replay.
    pub async fn approve_follow_from(&self, follower: &str) {
        let pending = self
            .svc
            .pending_requests(&self.db)
            .await
            .unwrap_or_else(|e| panic!("[{}] pending requests failed: {e}", self.name));
        let request = pending
            .iter()
            .find(|f| f.follower_node_id == follower)
            .unwrap_or_else(|| panic!("[{}] no pending follow from {follower}", self.name));
        self.svc
            .resolve_follow(&self.db, request.id, "approve", None, None)
            .await
            .unwrap_or_else(|e| panic!("[{}] approval failed: {e}", self.name));
    }

    /// Reads `owner`'s catalog from the hub, opening it with our wrap when
    /// it is encrypted.
    pub async fn read_catalog(
        &self,
        owner: &Instance,
    ) -> Result<Vec<CatalogEntry>, rust_lib_app::services::hub_directory_service::HubDirectoryError>
    {
        use zeroize::Zeroize;
        let (mut ed25519_bytes, mut x25519_bytes) = self
            .state
            .identity_service
            .identity()
            .expect("identity")
            .export_secret_bytes();
        let secret = x25519_dalek::StaticSecret::from(x25519_bytes);
        ed25519_bytes.zeroize();
        x25519_bytes.zeroize();
        self.svc
            .get_catalog(&self.db, owner.node_id(), Some(&secret))
            .await
    }

    /// Forgets the last confirmed push, as the recipe's A12 does by hand:
    /// the next sync contacts the hub again with the same catalog.
    pub async fn forget_push_state(&self) {
        use sea_orm::{ConnectionTrait, Statement};
        self.db
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "UPDATE hub_directory_config
                    SET last_catalog_hash = NULL, last_catalog_pushed_at = NULL"
                    .to_string(),
            ))
            .await
            .expect("push state reset");
    }

    /// Removes a peer, the owner's gesture that ends a pairing.
    pub async fn delete_peer(&self, peer_id: i32) {
        let app = Router::new()
            .route("/peers/:id", delete(api::peer::delete_peer))
            .with_state(self.state.clone());
        self.owner_call(app, "DELETE", &format!("/peers/{peer_id}"), None)
            .await;
    }

    /// Accepts a pending peer, the owner's gesture that restores an ended
    /// pairing.
    pub async fn accept_peer(&self, peer_id: i32) {
        let app = Router::new()
            .route("/peers/:id/status", put(api::peer::update_peer_status))
            .with_state(self.db.clone());
        self.owner_call(
            app,
            "PUT",
            &format!("/peers/{peer_id}/status"),
            Some(serde_json::json!({ "status": "accepted" })),
        )
        .await;
    }

    /// Sends an E2EE message to `peer`, direct first then by relay, waiting
    /// at most `relay_wait` for a relayed answer.
    pub async fn send_e2ee(
        &self,
        peer: &peer::Model,
        message_type: &str,
        payload: serde_json::Value,
        relay_wait: Duration,
    ) -> Result<Option<Option<ClearMessage>>, String> {
        api::peer::try_send_e2ee_with_timeout(&self.state, peer, message_type, payload, relay_wait)
            .await
    }

    /// One relay poll cycle, as the app's timer or a hub nudge would run.
    pub async fn poll_relay(&self) {
        relay_poller::poll_once(&self.state, NudgeSource::Manual)
            .await
            .unwrap_or_else(|e| panic!("[{}] relay poll failed: {e}", self.name));
    }

    pub async fn relay_config(&self) -> Option<relay_config::Model> {
        relay_config::Entity::find().one(&self.db).await.unwrap()
    }

    /// Registers the library on the hub with its keys and relay mailbox,
    /// as the app does after the identity and the relay are ready.
    pub async fn register(
        &self,
        display_name: &str,
        listed: bool,
        requires_approval: bool,
    ) -> DirectoryConfig {
        let x25519 = self.x25519_public_hex();
        let relay = self.relay_config().await;
        let params = RegisterParams {
            node_id: self.library_uuid.clone(),
            display_name: display_name.to_string(),
            is_listed: Some(listed),
            requires_approval: Some(requires_approval),
            accept_from: "everyone".to_string(),
            description: None,
            location_country: None,
            location_city_id: None,
            allow_borrowing: None,
            x25519_public_key: Some(x25519),
            website: None,
            device_model: Some("two-instance test".to_string()),
            device_fingerprint: Some(format!("it-{}", self.library_uuid)),
            app_version: Some("1.3.0".to_string()),
            relay_url: relay.as_ref().map(|r| r.relay_url.clone()),
            relay_mailbox_id: relay.as_ref().map(|r| r.mailbox_uuid.clone()),
            relay_write_token: relay.as_ref().map(|r| r.write_token.clone()),
            avatar_config: None,
        };
        self.svc
            .register_or_update(&self.db, params)
            .await
            .unwrap_or_else(|e| panic!("[{}] register failed: {e}", self.name))
    }

    /// Pushes `entries` to the hub, in clear or under the catalog key, the
    /// way the FFI sync does.
    pub async fn push(
        &self,
        entries: Vec<CatalogEntry>,
        encrypted: bool,
    ) -> (i32, PushCatalogOutcome) {
        let catalog = HubCatalogToPush {
            book_count: entries.len() as i64,
            entries,
            cover_book_ids: vec![],
        };
        let mode = if encrypted {
            CatalogSyncMode::Encrypted {
                store: &self.key_store,
                library_uuid: &self.library_uuid,
            }
        } else {
            CatalogSyncMode::Clear
        };
        self.svc
            .sync_catalog(&self.db, &catalog, mode)
            .await
            .unwrap_or_else(|e| panic!("[{}] catalog sync failed: {e}", self.name))
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A catalog entry with a fresh book id.
pub fn entry(isbn: &str, title: &str) -> CatalogEntry {
    CatalogEntry {
        isbn: isbn.to_string(),
        book_id: Some(uuid::Uuid::new_v4().to_string()),
        title: title.to_string(),
        author: None,
        cover_url: None,
        added_at: None,
    }
}
