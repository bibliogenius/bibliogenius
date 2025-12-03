use axum::{
    routing::{get, post, put},
    Router,
};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use bibliogenius::{api, config, db, seed};

/// Find an available port starting from the preferred port
fn find_available_port(preferred_port: u16) -> Option<u16> {
    // Try preferred port first
    if TcpListener::bind(("127.0.0.1", preferred_port)).is_ok() {
        return Some(preferred_port);
    }

    // Scan next 100 ports
    for port in (preferred_port + 1)..(preferred_port + 100) {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }

    None
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
        .with(tracing_subscriber::fmt::layer())
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

    // Build API router
    let api_router = Router::new()
        // Health check
        .route("/health", get(api::health::health_check))
        // Auth
        .route("/auth/login", post(api::auth::login))
        .route("/auth/register", post(api::auth::create_admin))
        // Library config
        .route("/library/config", get(api::library::get_config))
        .route("/library/config", post(api::library::update_config))
        // Books
        .route("/books", get(api::books::list_books))
        .route("/api/books/search", get(api::search::search_books))
        .route("/api/chat", post(api::chat::chat_handler))
        .route("/books", post(api::books::create_book))
        .route(
            "/books/:id",
            axum::routing::put(api::books::update_book).delete(api::books::delete_book),
        )
        // Authors
        .route("/authors", get(api::author::list_authors))
        .route("/authors", post(api::author::create_author))
        .route("/authors/:id", get(api::author::get_author))
        .route(
            "/authors/:id",
            axum::routing::delete(api::author::delete_author),
        )
        // Tags
        .route("/tags", get(api::tag::list_tags))
        .route("/tags", post(api::tag::create_tag))
        .route("/tags/:id", get(api::tag::get_tag))
        .route("/tags/:id", axum::routing::delete(api::tag::delete_tag))
        // Peers
        .route("/peers", get(api::peer::list_peers))
        .route("/peers/connect", post(api::peer::connect))
        .route("/peers/push", post(api::peer::push_operations))
        .route("/peers/pull", get(api::peer::pull_operations))
        .route("/peers/:id/sync", post(api::peer::sync_peer)) // Sync remote books by ID
        .route("/peers/sync_by_url", post(api::peer::sync_peer_by_url)) // Sync by URL (solves Hub ID mismatch)
        .route("/peers/:id/books", get(api::peer::list_peer_books))
        .route(
            "/peers/books_by_url",
            post(api::peer::list_peer_books_by_url),
        ) // Get books by URL
        .route("/peers/search", post(api::peer::search_local))
        .route("/peers/proxy_search", post(api::peer::proxy_search))
        .route("/peers/:id/request", post(api::peer::request_book)) // Send request
        .route("/peers/request", post(api::peer::receive_request)) // Receive request
        .route("/peers/requests", get(api::peer::list_requests)) // List incoming requests
        .route(
            "/peers/requests/outgoing",
            get(api::peer::list_outgoing_requests),
        ) // List outgoing requests
        .route(
            "/peers/requests/outgoing/:id",
            axum::routing::delete(api::peer::delete_outgoing_request),
        )
        .route("/peers/requests/:id", put(api::peer::update_request_status)) // Update status
        .route(
            "/peers/requests/:id",
            axum::routing::delete(api::peer::delete_request),
        ) // Delete request
        // Scanning
        .route("/scan/image", post(api::scan::scan_image))
        // Batch Operations
        .route("/books/batch/edit", post(api::batch::batch_edit))
        .route("/books/batch/sort", post(api::batch::batch_sort))
        .route("/books/duplicates", get(api::batch::find_duplicates))
        // Copies
        // Copies
        .route("/copies", get(api::copy::list_copies))
        .route("/copies", post(api::copy::create_copy))
        .route("/books/:id/copies", get(api::copy::get_book_copies))
        .route("/copies/:id", axum::routing::delete(api::copy::delete_copy))
        // Contacts
        .route(
            "/contacts",
            get(api::contact::list_contacts).post(api::contact::create_contact),
        )
        .route(
            "/contacts/:id",
            get(api::contact::get_contact)
                .put(api::contact::update_contact)
                .delete(api::contact::delete_contact),
        )
        .route("/profile", put(api::profile::update_profile))
        // P2P routes
        .route(
            "/loans",
            get(api::loan::list_loans).post(api::loan::create_loan),
        )
        .route("/loans/:id/return", put(api::loan::return_loan))
        // Lookup
        .route("/lookup/:isbn", get(api::lookup::lookup_book))
        // Data Import/Export
        .route("/import/file", axum::routing::post(api::data::import_file))
        // Setup & Config
        .route("/setup", axum::routing::post(api::setup::setup))
        .route("/config", get(api::setup::get_config))
        // Integrations (Professional)
        .route(
            "/integrations/sudoc/search",
            get(api::integrations::search_sudoc),
        )
        .route(
            "/integrations/osm/libraries",
            get(api::integrations::search_osm_libraries),
        )
        .route(
            "/integrations/osm/bookstores",
            get(api::integrations::search_osm_bookstores),
        )
        .route(
            "/integrations/openlibrary/search",
            get(api::integrations::search_openlibrary),
        )
        // Gamification
        .route("/user/status", get(api::gamification::get_user_status))
        // Export
        .route("/export", get(api::export::export_data))
        .with_state(db);

    // Swagger UI
    use bibliogenius::api_docs::ApiDoc;
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
    let port = find_available_port(config.port)
        .expect("Failed to find available port");
    
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
