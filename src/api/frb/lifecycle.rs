// Backend startup and diagnostics: init_backend, hub URL, health, version, log tail.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Initialization ============

/// Initialize the FFI backend with database at the given path
/// Must be called before any other FFI functions
pub async fn init_backend(db_path: String) -> Result<String, String> {
    // Install panic hook first thing to catch any panics
    install_panic_hook();

    // Initialize tracing for FFI mode (debug builds only).
    // Release builds produce no log output to avoid leaking sensitive data.
    // Filter targets the lib crate name "rust_lib_app", not the package name.
    //
    // Log file lives next to the SQLite DB (parent of `db_path`):
    //   macOS: ~/Library/Application Support/com.bibliogenius.app/bibliogenius-rust.log
    //   iOS:   <app sandbox>/Documents/bibliogenius-rust.log
    // Both are writable and retrievable via `get_rust_log_tail` FFI.
    // Truncated at each init so the file does not grow indefinitely across
    // launches - within a single session tracing keeps appending.
    let log_path = std::path::Path::new(&db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("bibliogenius-rust.log");
    let _ = LOG_PATH.set(log_path.clone());

    // Register the covers directory (sibling of the DB file) so the
    // peer-facing cover endpoint can re-base persisted absolute paths after an
    // iOS data-container UUID change. Mirrors the Flutter `LocalCoverResolver`.
    let covers_dir = std::path::Path::new(&db_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("covers");
    let _ = COVERS_DIR.set(covers_dir);

    static TRACING_INIT: std::sync::Once = std::sync::Once::new();
    TRACING_INIT.call_once(|| {
        if cfg!(debug_assertions) {
            // Also enable the `ssrf` target family (ADR-026) so SSRF audit
            // events are always visible in debug builds - they live outside
            // the `rust_lib_app` namespace so they would otherwise be dropped.
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust_lib_app=info,ssrf=warn".into());

            if let Ok(file) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&log_path)
            {
                let _ = tracing_subscriber::registry()
                    .with(filter)
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_writer(std::sync::Mutex::new(file))
                            .with_ansi(false),
                    )
                    .try_init();
            } else {
                let _ = tracing_subscriber::registry()
                    .with(filter)
                    .with(tracing_subscriber::fmt::layer().with_ansi(false))
                    .try_init();
            }
        }
    });

    if DB.get().is_some() {
        return Ok("Already initialized".to_string());
    }

    let db_url = format!("sqlite:{}?mode=rwc", db_path);

    // Set the DATABASE_URL environment variable so that other components (like MCP config)
    // can access the correct database path being used by the FFI instance.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("DATABASE_URL", &db_url) };
    tracing::info!("FFI: DATABASE_URL configured");

    // Account-sync builds open a single cr-sqlite connection with the replicated
    // tables promoted to CRRs; default builds use a plain pool.
    #[cfg(feature = "account_sync")]
    let init = crate::db::init_db_account_sync(&db_url).await;
    #[cfg(not(feature = "account_sync"))]
    let init = crate::db::init_db(&db_url).await;

    match init {
        Ok(conn) => match DB.set(conn) {
            Ok(_) => {
                // ADR-037 §5: purge expired rollback siblings from prior
                // restores. Best-effort; never blocks startup on FS errors.
                crate::infrastructure::db::run_startup_maintenance(std::path::Path::new(&db_path));
                Ok("Backend initialized successfully".to_string())
            }
            Err(_) => Err("Failed to set database connection".to_string()),
        },
        Err(e) => Err(format!("Database initialization failed: {}", e)),
    }
}

// ============ Hub URL ============

/// Pass the hub URL from Flutter to the Rust process environment.
/// Must be called once after init_backend, before any hub_directory calls.
/// Rust reads HUB_URL via std::env::var - it cannot see Flutter's dotenv map.
///
/// The .env value is only a default: if a relay has been configured (persisted
/// in `my_relay_config`), its URL takes precedence so the hub directory and
/// relay always point to the same hub.
pub async fn set_hub_url_ffi(hub_url: String) -> Result<(), String> {
    // Prioritize persisted relay URL over .env default.
    let effective_url = if let Ok(db_ref) = hub_db() {
        crate::api::relay::get_my_relay_config(db_ref)
            .await
            .map(|c| c.relay_url)
            .unwrap_or_else(|| hub_url.clone())
    } else {
        hub_url.clone()
    };

    // If the relay URL overrides the .env default, the directory config
    // (write_token) was issued by the .env hub and is invalid on the
    // relay hub. Invalidate so ensureRegistered() re-registers.
    // A trailing-slash difference alone must not count as "different hub",
    // otherwise we burn the write_token on every startup; use the shared
    // comparator that also guards the setup_relay path.
    if crate::utils::hub_url::hub_urls_differ(&hub_url, &effective_url)
        && let Ok(db_ref) = hub_db()
    {
        use sea_orm::ConnectionTrait;
        let _ = db_ref
            .execute(sea_orm::Statement::from_string(
                db_ref.get_database_backend(),
                "DELETE FROM hub_directory_config".to_owned(),
            ))
            .await;
        tracing::info!(
            "Hub URL differs from .env ({hub_url} -> {effective_url}), directory config invalidated"
        );
    }

    // SAFETY: single-threaded init path, same pattern as DATABASE_URL above.
    unsafe { std::env::set_var("HUB_URL", &effective_url) };
    tracing::info!("HUB_URL set to {effective_url}");
    Ok(())
}

// ============ Health Check ============

/// Check if the FFI backend is healthy
#[frb(sync)]
pub fn health_check() -> String {
    if DB.get().is_some() {
        "OK".to_string()
    } else {
        "NOT_INITIALIZED".to_string()
    }
}

/// Get the FFI backend version
#[frb(sync)]
pub fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Return the last `lines` lines of the Rust tracing log file.
/// Empty string if the file does not exist, tracing is disabled (release
/// build), or `init_backend` has not run yet.
#[frb(sync)]
pub fn get_rust_log_tail(lines: u32) -> String {
    let Some(path) = LOG_PATH.get() else {
        return String::new();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines as usize);
    all[start..].join("\n")
}

/// Simple greeting function to test the bridge
#[frb(sync)]
pub fn greet(name: String) -> String {
    format!("Hello, {}! Welcome to BiblioGenius.", name)
}
