use axum::Router;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use rust_lib_app::{api, config, db, seed};

/// Find an available port starting from the preferred port
fn find_available_port(preferred_port: u16) -> Option<u16> {
    // Try preferred port first
    if TcpListener::bind(("0.0.0.0", preferred_port)).is_ok() {
        return Some(preferred_port);
    }

    // Scan next 100 ports
    ((preferred_port + 1)..(preferred_port + 100))
        .find(|&port| TcpListener::bind(("0.0.0.0", port)).is_ok())
}

/// Write the selected port to a file for the Flutter app to read
fn write_port_file(port: u16) -> std::io::Result<()> {
    let port_file = get_port_file_path();

    // Create parent directory if it doesn't exist
    if let Some(parent) = port_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(port_file, port.to_string())
}

/// Get the path to the port file
fn get_port_file_path() -> PathBuf {
    // On macOS: ~/Library/Caches/BiblioGenius/backend_port.txt
    // On Linux: ~/.cache/bibliogenius/backend_port.txt
    // On Windows: %LOCALAPPDATA%\BiblioGenius\backend_port.txt

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").expect("HOME not set");
        PathBuf::from(home)
            .join("Library")
            .join("Caches")
            .join("BiblioGenius")
            .join("backend_port.txt")
    }

    #[cfg(target_os = "linux")]
    {
        let home = std::env::var("HOME").expect("HOME not set");
        PathBuf::from(home)
            .join(".cache")
            .join("bibliogenius")
            .join("backend_port.txt")
    }

    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("LOCALAPPDATA").expect("LOCALAPPDATA not set");
        PathBuf::from(appdata)
            .join("BiblioGenius")
            .join("backend_port.txt")
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "bibliogenius=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    // Load configuration
    dotenvy::dotenv().ok();
    let config = config::Config::from_env();

    // Initialize database
    let db = db::init_db(&config.database_url)
        .await
        .expect("Failed to initialize database");

    // Check for seed flag
    if std::env::var("SEED_DEMO").is_ok() {
        tracing::info!("Seeding demo data...");
        if let Err(e) = seed::seed_demo_data(&db).await {
            tracing::error!("Failed to seed data: {}", e);
        } else {
            tracing::info!("Demo data seeded successfully.");
        }
    }

    // [MCP] Start MCP Server if --mcp flag is present
    #[cfg(feature = "mcp")]
    {
        if std::env::args().any(|arg| arg == "--mcp") {
            tracing::info!("Starting in MCP Mode (Stdio)...");
            api::mcp::start_server(db).await;
            return;
        }
    }

    // [P2P] Start Operation Processor
    let processor_db = db.clone();
    tokio::spawn(async move {
        // We use the fully qualified path to ensure we hit the right module
        rust_lib_app::sync::processor::run_processor(processor_db).await;
    });

    // Build API router
    let api_router = api::api_router(db);

    // Swagger UI
    use rust_lib_app::api_docs::ApiDoc;
    use utoipa::OpenApi;
    use utoipa_swagger_ui::SwaggerUi;

    let app = Router::new()
        .merge(SwaggerUi::new("/api/docs").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .nest("/api", api_router)
        .nest_service("/", ServeDir::new("static"))
        // CORS
        .layer(
            CorsLayer::new()
                .allow_origin(
                    config
                        .cors_allowed_origins
                        .iter()
                        .map(|origin| origin.parse::<axum::http::HeaderValue>().unwrap())
                        .collect::<Vec<_>>(),
                )
                .allow_methods(Any)
                .allow_headers(Any),
        );

    // Find available port
    let port = find_available_port(config.port).expect("Failed to find available port");

    if port != config.port {
        tracing::warn!(
            "Preferred port {} was not available, using port {} instead",
            config.port,
            port
        );
    }

    // Write port to file for Flutter app
    if let Err(e) = write_port_file(port) {
        tracing::error!("Failed to write port file: {}", e);
    } else {
        tracing::info!("Port file written: {:?}", get_port_file_path());
    }

    // Initialize mDNS for local network discovery (if enabled)
    let mdns_enabled = std::env::var("MDNS_ENABLED")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true); // Enabled by default

    if mdns_enabled {
        // Get library name from database or use default
        let library_name =
            std::env::var("LIBRARY_NAME").unwrap_or_else(|_| "BiblioGenius Library".to_string());

        match rust_lib_app::services::init_mdns(&library_name, port, None) {
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
        tracing::info!("mDNS disabled via MDNS_ENABLED=false");
    }

    // Start server
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("BiblioGenius server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    axum::serve(listener, app)
        .await
        .expect("Failed to start server");
}
