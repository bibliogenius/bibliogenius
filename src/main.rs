use axum::{
    routing::{get, post, put},
    Router,
};
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod api;
mod config;
mod db;
mod models;
mod auth;
mod seed;
mod sync;

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
        // Library config
        .route("/library/config", get(api::library::get_config))
        .route("/library/config", post(api::library::update_config))
        // Books
        .route("/books", get(api::books::list_books))
        .route("/books", post(api::books::create_book))
        .route("/books/:id", axum::routing::delete(api::books::delete_book))
        // Authors
        .route("/authors", get(api::author::list_authors))
        .route("/authors", post(api::author::create_author))
        .route("/authors/:id", get(api::author::get_author))
        .route("/authors/:id", axum::routing::delete(api::author::delete_author))
        // Tags
        .route("/tags", get(api::tag::list_tags))
        .route("/tags", post(api::tag::create_tag))
        .route("/tags/:id", get(api::tag::get_tag))
        .route("/tags/:id", axum::routing::delete(api::tag::delete_tag))
        // Peers
        .route("/peers/connect", post(api::peer::connect))
        .route("/peers/push", post(api::peer::push_operations))
        .route("/peers/pull", get(api::peer::pull_operations))
        .route("/peers/search", post(api::peer::search_local))
        .route("/peers/proxy_search", post(api::peer::proxy_search))
        // Copies
        .route("/copies", get(api::copy::list_copies))
        .route("/copies", post(api::copy::create_copy))
        .route("/books/:id/copies", get(api::copy::get_book_copies))
        .route("/copies/:id", axum::routing::delete(api::copy::delete_copy))
        // Contacts
        .route("/contacts", get(api::contact::list_contacts).post(api::contact::create_contact))
        .route("/contacts/:id", get(api::contact::get_contact).put(api::contact::update_contact).delete(api::contact::delete_contact))
        // Loan routes
        .route("/loans", get(api::loan::list_loans).post(api::loan::create_loan))
        .route("/loans/:id/return", put(api::loan::return_loan))
        .with_state(db);

    // Build main app with static file serving
    let app = Router::new()
        .nest("/api", api_router)
        .nest_service("/", ServeDir::new("static"))
        // CORS
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    // Start server
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("BiblioGenius server v2 (Loans) listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    axum::serve(listener, app)
        .await
        .expect("Failed to start server");
}
