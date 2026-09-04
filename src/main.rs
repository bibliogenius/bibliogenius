use axum::Router;
use axum::routing::get;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use sea_orm::EntityTrait;

use rust_lib_app::{api, config, db, seed};

/// Bind the preferred port, or the first free one in the 100 that follow.
///
/// The listener is returned bound, not just probed: the port file and the mDNS
/// announcement are derived from it before serving starts, so the port must
/// not be up for grabs in between (port 8000 is regularly taken by Docker or a
/// second instance of the app). A preferred port of 0 asks the OS for an
/// ephemeral port; read the real one from `local_addr()`.
fn find_available_port(preferred_port: u16) -> Option<TcpListener> {
    // Try preferred port first
    if let Ok(listener) = TcpListener::bind(("0.0.0.0", preferred_port)) {
        return Some(listener);
    }

    // Scan next 100 ports, stopping at the top of the range instead of wrapping
    (preferred_port.saturating_add(1)..=preferred_port.saturating_add(100))
        .find_map(|port| TcpListener::bind(("0.0.0.0", port)).ok())
}

/// A profile names the port file and the database file, so it must stay a plain
/// token: `--profile ../x` would otherwise write outside the cache directory.
fn is_valid_profile(profile: &str) -> bool {
    !profile.is_empty()
        && profile.len() <= 32
        && profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether an opt-in environment flag is enabled: set, and not an explicit
/// "off" value (empty, `0` or `false`, case-insensitive).
fn env_flag_enabled(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        None | Some("") | Some("0") => false,
        Some(v) => !v.eq_ignore_ascii_case("false"),
    }
}

/// Write the selected port to a file for tooling that talks to the running
/// server (`scripts/qa_delta_sync.sh`); the Flutter app goes through FFI and
/// never reads it. Written to a sibling temp file (unique per process, so two
/// instances on the same profile never share it) then renamed, so a reader
/// never sees a truncated file. The file is not removed at exit: a crash would
/// leave it behind anyway, so readers must treat a dead port as stale.
fn write_port_file_at(port_file: &std::path::Path, port: u16) -> std::io::Result<()> {
    // Create parent directory if it doesn't exist
    if let Some(parent) = port_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_file = port_file.with_extension(format!("txt.{}.tmp", std::process::id()));
    std::fs::write(&tmp_file, port.to_string())?;
    std::fs::rename(tmp_file, port_file)
}

/// Get the path to the port file. Fails instead of panicking when the base
/// directory variable is missing, so the caller logs it like any other write error.
fn get_port_file_path(profile: &str) -> std::io::Result<PathBuf> {
    let filename = if profile == "default" {
        "backend_port.txt".to_string()
    } else {
        format!("backend_port_{}.txt", profile)
    };
    // On macOS: ~/Library/Caches/BiblioGenius/backend_port.txt
    // (under the App Sandbox, HOME is the app container, not the user's home)
    // On Linux: ~/.cache/bibliogenius/backend_port.txt
    // On Windows: %LOCALAPPDATA%\BiblioGenius\backend_port.txt

    #[cfg(target_os = "macos")]
    let base = base_dir_from_env("HOME")?
        .join("Library")
        .join("Caches")
        .join("BiblioGenius");

    #[cfg(target_os = "linux")]
    let base = base_dir_from_env("HOME")?
        .join(".cache")
        .join("bibliogenius");

    #[cfg(target_os = "windows")]
    let base = base_dir_from_env("LOCALAPPDATA")?.join("BiblioGenius");

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    compile_error!("get_port_file_path: add a port file location for this target OS");

    Ok(base.join(filename))
}

fn base_dir_from_env(var: &str) -> std::io::Result<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{var} is not set, cannot locate the port file"),
        )
    })
}

/// Log loudly when a worker that should run forever stops, whether it panicked or
/// returned. Nothing restarts it: sync being dead must at least be visible.
fn supervise_worker(name: &'static str, handle: tokio::task::JoinHandle<()>) {
    tokio::spawn(async move {
        match handle.await {
            Ok(()) => tracing::error!("Background worker '{}' stopped unexpectedly", name),
            Err(e) => tracing::error!("Background worker '{}' died: {}", name, e),
        }
    });
}

/// Parse the configured CORS origins, skipping the invalid ones so a typo never
/// blocks startup. `*` is refused: `AllowOrigin::list` panics on it, and the
/// router allows any method and header, so a wildcard origin would open every
/// non owner-gated route to any site.
fn parse_cors_origins(origins: &[String]) -> Vec<axum::http::HeaderValue> {
    let mut parsed = Vec::new();
    for origin in origins {
        if origin == "*" {
            tracing::error!(
                "CORS_ALLOWED_ORIGINS does not accept '*': list the origins explicitly (ignored)"
            );
            continue;
        }
        match origin.parse::<axum::http::HeaderValue>() {
            Ok(v) => parsed.push(v),
            Err(e) => tracing::error!("Failed to parse CORS origin '{}': {}", origin, e),
        }
    }
    parsed
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    // Filter targets the lib crate name "rust_lib_app" (not the package name "bibliogenius").
    // The `ssrf` target family (ADR-026) is explicitly included so SSRF audit events
    // are always emitted: they live outside the `rust_lib_app` namespace.
    // Debug builds default to `debug`; the shipped binary to `info`, like the FFI
    // side, so a future `debug!` cannot leak into users' logs. RUST_LOG still wins.
    #[cfg(debug_assertions)]
    const DEFAULT_LOG_FILTER: &str = "rust_lib_app=debug,tower_http=debug,ssrf=warn";
    #[cfg(not(debug_assertions))]
    const DEFAULT_LOG_FILTER: &str = "rust_lib_app=info,tower_http=info,ssrf=warn";
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    // Load configuration
    dotenvy::dotenv().ok();

    // `--profile` overrides the PROFILE variable. It is passed to the config rather
    // than written back into the environment: `set_var` is not thread-safe and the
    // runtime's worker threads already exist here.
    let args: Vec<String> = std::env::args().collect();
    let profile_arg = args
        .iter()
        .position(|arg| arg == "--profile")
        .and_then(|pos| args.get(pos + 1))
        .map(String::as_str);

    let config = config::Config::from_env_with_profile(profile_arg);

    // [MCP] Short-circuit before the database init: the helper is a transport shim
    // that proxies to the running app and never opens the database itself. Opening
    // the same SQLite file from a second process was the root of a whole class of
    // bugs (sandbox paths, CRR promotion, feature mismatch), so it no longer happens.
    #[cfg(feature = "mcp")]
    {
        if std::env::args().any(|arg| arg == "--mcp") {
            tracing::info!("Starting in MCP Mode (Stdio)...");
            api::mcp::start_server().await;
            return;
        }
    }

    // The profile names the database and the port file, neither of which the MCP
    // helper above touches, so it is checked here rather than before the short-circuit.
    if !is_valid_profile(&config.profile) {
        tracing::error!(
            "Invalid profile '{}': use 1 to 32 characters among letters, digits, '_' and '-'",
            config.profile
        );
        std::process::exit(2);
    }

    // Initialize database. Account-sync builds open a single cr-sqlite connection
    // with the replicated tables promoted to CRRs; default builds use a plain pool.
    #[cfg(feature = "account_sync")]
    let db = db::init_db_account_sync(&config.database_url)
        .await
        .expect("Failed to initialize database");
    #[cfg(not(feature = "account_sync"))]
    let db = db::init_db(&config.database_url)
        .await
        .expect("Failed to initialize database");

    // Check for seed flag
    if env_flag_enabled(std::env::var("SEED_DEMO").ok().as_deref()) {
        tracing::info!("Seeding demo data...");
        match seed::seed_demo_data(&db).await {
            Err(e) => {
                tracing::error!("Failed to seed data: {}", e);
            }
            _ => {
                tracing::info!("Demo data seeded successfully.");
            }
        }
    }

    // [P2P] Start Operation Processor
    let processor_db = db.clone();
    let processor = tokio::spawn(async move {
        // We use the fully qualified path to ensure we hit the right module
        rust_lib_app::sync::processor::run_processor(processor_db).await;
    });

    // [Delta sync] Hybrid retention pruner for operation_log (ADR-028 D5).
    rust_lib_app::services::oplog_pruner::spawn(db.clone());

    // Build API router with explicit AppState (needed for relay poller)
    let state = rust_lib_app::infrastructure::AppState::new(db);
    let api_router = api::api_router_with_state(state.clone());

    // Spawn relay poller (checks for incoming relay messages in the background)
    let relay_poller = {
        let poller_state = state.clone();
        tokio::spawn(async move {
            rust_lib_app::services::relay_poller::start_relay_polling(
                poller_state,
                std::time::Duration::from_secs(20),
            )
            .await;
        })
    };

    // Spawn WS nudge listener (instant relay notifications via WebSocket, ADR-017)
    let ws_nudge = {
        let ws_state = state.clone();
        tokio::spawn(async move {
            rust_lib_app::services::ws_nudge::start_ws_nudge(ws_state).await;
        })
    };

    // These workers loop for the life of the process. A panic inside one ends its
    // task silently while HTTP keeps answering, so watch the handles and shout.
    // The oplog pruner spawns its own task and drops its handle; not covered here.
    supervise_worker("operation processor", processor);
    supervise_worker("relay poller", relay_poller);
    supervise_worker("ws nudge listener", ws_nudge);

    // Swagger UI
    use rust_lib_app::api_docs::ApiDoc;
    use utoipa::OpenApi;
    use utoipa_swagger_ui::SwaggerUi;

    let cors_allowed_origins = parse_cors_origins(&config.cors_allowed_origins);

    let app = Router::new()
        .merge(SwaggerUi::new("/api/docs").url("/api-docs/openapi.json", ApiDoc::openapi()))
        // Invite landing page at root level (not under /api)
        // Serves HTML redirect to bibliogenius:// custom scheme
        .route("/invite", get(api::invite_page::invite_page))
        .nest("/api", api_router)
        // CORS
        .layer(
            CorsLayer::new()
                .allow_origin(cors_allowed_origins)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    // Bind the port now and keep the listener: everything announced below refers to it
    let std_listener = find_available_port(config.port).expect("Failed to find available port");
    let port = std_listener
        .local_addr()
        .expect("Failed to read the bound address")
        .port();

    if port != config.port {
        tracing::warn!(
            "Preferred port {} was not available, using port {} instead",
            config.port,
            port
        );
    }

    // Write the port file for tooling (see `write_port_file_at`)
    let written = get_port_file_path(&config.profile)
        .and_then(|path| write_port_file_at(&path, port).map(|()| path));
    match written {
        Ok(path) => tracing::info!("Port file written: {}", path.display()),
        Err(e) => tracing::error!("Failed to write port file: {}", e),
    }

    // Initialize mDNS for local network discovery (if enabled)
    let mdns_enabled = env_flag_enabled(std::env::var("MDNS_ENABLED").ok().as_deref()); // Opt-in

    if mdns_enabled {
        // Single-row config table, still on an integer primary key.
        let library_name = match rust_lib_app::models::library_config::Entity::find_by_id(1)
            .one(state.db())
            .await
        {
            Ok(Some(config)) => config.name,
            Ok(None) => {
                tracing::warn!("No library config row: announcing mDNS under a generic name");
                "BiblioGenius Library".to_string()
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read the library name ({}): announcing mDNS under a generic name",
                    e
                );
                "BiblioGenius Library".to_string()
            }
        };

        match rust_lib_app::services::init_mdns(&library_name, port, None, None, None) {
            Ok(()) => {
                tracing::info!("📡 mDNS service started - library discoverable on local network");
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to start mDNS service: {} (local discovery disabled)",
                    e
                );
            }
        }
    } else {
        tracing::info!("mDNS disabled (opt in with MDNS_ENABLED=true)");
    }

    // Start server
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("BiblioGenius server listening on {}", addr);

    std_listener
        .set_nonblocking(true)
        .expect("Failed to set the listener non-blocking");
    let listener =
        tokio::net::TcpListener::from_std(std_listener).expect("Failed to register the listener");

    let serve = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );

    // On account-sync builds the pool is a single cr-sqlite connection. Serve until a
    // shutdown signal, then finalize.
    //
    // This is the one place a bare `finalize` is legitimate: the future below has
    // returned, so HTTP no longer serves, and the process exits immediately after.
    // The background workers above are still running and share the connection; the
    // pool is single-connection, so `finalize` waits for any open transaction, and
    // whatever they attempt afterwards fails with a warning until the exit.
    // Everywhere else the connection outlives the call, and finalizing it wedges
    // every later merge (see `crsqlite_crr::finalize`); use `finalize_and_close`
    // there instead.
    #[cfg(feature = "account_sync")]
    {
        serve
            .with_graceful_shutdown(shutdown_signal())
            .await
            .expect("Failed to start server");
        if let Err(e) = rust_lib_app::infrastructure::crsqlite_crr::finalize(state.db()).await {
            tracing::warn!("crsql_finalize on shutdown failed: {}", e);
        }
    }

    #[cfg(not(feature = "account_sync"))]
    serve.await.expect("Failed to start server");
}

/// Resolve when the process receives a shutdown signal: Ctrl-C (SIGINT) or, on
/// Unix, SIGTERM (the signal `docker stop` / systemd send). Used to drive the
/// account-sync graceful shutdown so `crsql_finalize` runs on the normal stop path,
/// not only on an interactive Ctrl-C.
#[cfg(feature = "account_sync")]
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // If the handler cannot be installed, never resolve on this arm: Ctrl-C
            // still triggers shutdown.
            Err(e) => {
                tracing::warn!("Failed to install SIGTERM handler: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_available_port_does_not_overflow_at_the_top_of_the_range() {
        // Hold the top port so the scan branch runs; if something else already
        // holds it, that serves the same purpose.
        let _guard = TcpListener::bind(("0.0.0.0", u16::MAX));
        // Must not panic: the scan stops at u16::MAX instead of wrapping.
        let _ = find_available_port(u16::MAX);
    }

    #[test]
    fn find_available_port_returns_a_bound_listener_past_an_occupied_port() {
        let occupied = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let taken = occupied.local_addr().unwrap().port();

        let listener = find_available_port(taken).expect("a free port in the range");
        let chosen = listener.local_addr().unwrap().port();

        assert_ne!(chosen, taken);
        // The port stays held by the returned listener, so a second bind fails.
        assert!(TcpListener::bind(("0.0.0.0", chosen)).is_err());
    }

    #[test]
    fn cors_wildcard_is_dropped_instead_of_reaching_allow_origin_list() {
        // `AllowOrigin::list` panics on `*` (tower-http 0.5); it must never get there.
        let parsed = parse_cors_origins(&["*".to_string(), "http://localhost:3000".to_string()]);
        assert_eq!(parsed, vec!["http://localhost:3000"]);
    }

    #[test]
    fn profile_must_be_a_plain_token() {
        assert!(is_valid_profile("default"));
        assert!(is_valid_profile("qa-2"));
        assert!(is_valid_profile("dev_A"));
        assert!(!is_valid_profile(""));
        assert!(!is_valid_profile("../../tmp/x"));
        assert!(!is_valid_profile("my profile"));
        assert!(!is_valid_profile(&"a".repeat(33)));
    }

    #[test]
    fn port_file_is_written_whole_with_no_temp_file_left() {
        let dir = std::env::temp_dir().join(format!("bg-port-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("backend_port.txt");

        write_port_file_at(&target, 12345).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "12345");
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "only the port file must remain, no temp file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_flag_rejects_explicit_false_values() {
        assert!(!env_flag_enabled(None));
        assert!(!env_flag_enabled(Some("")));
        assert!(!env_flag_enabled(Some("0")));
        assert!(!env_flag_enabled(Some("false")));
        assert!(!env_flag_enabled(Some("FALSE")));
        assert!(env_flag_enabled(Some("1")));
        assert!(env_flag_enabled(Some("true")));
    }
}
