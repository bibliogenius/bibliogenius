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
use axum::routing::post;
use rust_lib_app::api;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::relay_config;
use rust_lib_app::services::IdentityService;
use rust_lib_app::services::hub_catalog_key_store::CatalogKeyStore;
use rust_lib_app::services::hub_catalog_sync::{CatalogSyncMode, HubCatalogToPush};
use rust_lib_app::services::hub_directory_service::{
    CatalogEntry, DirectoryConfig, HubDirectoryService, PushCatalogOutcome, RegisterParams,
};
use sea_orm::{DatabaseConnection, EntityTrait};
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

    /// Creates a relay mailbox on the hub through the owner handler, the
    /// path "Connect" takes in the app. Returns the mailbox uuid only: the
    /// handler's body also carries the write token, which no test prints.
    pub async fn setup_relay(&self, hub: &Hub) -> String {
        let app = Router::new()
            .route("/relay/setup", post(api::peer::setup_relay))
            .with_state(self.state.clone());
        let body = serde_json::json!({ "relay_url": hub.url }).to_string();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/relay/setup")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        assert!(
            status.is_success(),
            "[{}] relay setup failed: {status} {json}",
            self.name
        );
        json["mailbox_uuid"]
            .as_str()
            .expect("mailbox uuid in the relay setup answer")
            .to_string()
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
        let (_, x25519) = self
            .state
            .identity_service
            .get_public_keys_hex()
            .expect("public keys");
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
