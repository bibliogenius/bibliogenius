// Global statics and runtime plumbing shared by every FFI handler.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// Global database connection (initialized once on app start)
static DB: OnceLock<DatabaseConnection> = OnceLock::new();
/// Path of the debug log file written by the tracing subscriber.
/// Set once in `init_backend`; read by `get_rust_log_tail` so Flutter can
/// display backend logs without relying on Xcode Console (stderr is invisible
/// to the iOS FFI host process).
static LOG_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
/// The application's covers directory (`<data dir>/covers`). Set once in
/// `init_backend` from the FFI-provided db path; read by `get_book_cover` to
/// re-base persisted cover paths after an iOS data-container UUID change.
/// `None` in server-binary mode where no FFI init runs.
static COVERS_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
/// Global AppState - set once in `initBackend`, read by FFI handlers that need
/// services not available as individual statics (e.g. catalog notifications).
static GLOBAL_APP_STATE: OnceLock<crate::infrastructure::AppState> = OnceLock::new();
#[allow(dead_code)]
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Get or create the tokio runtime
/// Uses current_thread runtime for iOS/mobile compatibility
#[allow(dead_code)]
fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        // Use current_thread runtime for mobile FFI compatibility
        // Multi-threaded runtime can cause issues on iOS
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| {
                eprintln!("FFI: Failed to create Tokio runtime: {}", e);
                // Create a minimal runtime as fallback
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("Failed to create even minimal Tokio runtime")
            })
    })
}

/// Install a panic hook to prevent crashes on iOS
/// This converts panics into logs instead of aborting
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|panic_info| {
            let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            let location = panic_info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown location".to_string());
            eprintln!("FFI PANIC at {}: {}", location, message);
        }));
    });
}

/// Get the database connection (must be initialized first)
fn db() -> Option<&'static DatabaseConnection> {
    DB.get()
}

/// Get the global AppState (must be initialized first via `initBackend`).
fn global_app_state() -> Option<&'static crate::infrastructure::AppState> {
    GLOBAL_APP_STATE.get()
}

/// The covers directory registered in `init_backend`, or `None` in
/// server-binary mode. Used by `get_book_cover` to re-base stored cover paths.
pub(crate) fn covers_dir() -> Option<&'static std::path::PathBuf> {
    COVERS_DIR.get()
}

/// Load the Google Books API key from the installation profile.
async fn load_google_books_api_key() -> Option<String> {
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    let db = db()?;
    if let Ok(Some(profile)) = ProfileEntity::find_by_id(1).one(db).await {
        let api_keys: std::collections::HashMap<String, String> = profile
            .api_keys
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        return api_keys.get("google_books").cloned();
    }
    None
}
