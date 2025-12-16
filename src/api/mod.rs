pub mod auth;
pub mod author;
pub mod batch;
pub mod books;
pub mod chat;
pub mod contact;
pub mod copy;
pub mod data;
pub mod export;
pub mod frb; // FFI API for flutter_rust_bridge
pub mod gamification;
pub mod genie;
pub mod health;
pub mod hub;
pub mod integrations;
pub mod library;
pub mod loan;
pub mod lookup;
pub mod peer;
pub mod profile;
pub mod scan;
pub mod search;
pub mod setup;
pub mod tag;
pub mod user;

use axum::{
    routing::{get, post, put},
    Router,
};
use sea_orm::DatabaseConnection;

pub fn api_router(db: DatabaseConnection) -> Router {
    Router::new()
        // Health check
        .route("/health", get(health::health_check))
        // Auth
        .route("/auth/login", post(auth::login))
        .route("/auth/login-mfa", post(auth::login_mfa))
        .route("/auth/register", post(auth::create_admin))
        .route("/auth/me", get(auth::get_me))
        .route("/auth/2fa/setup", post(auth::setup_2fa))
        .route("/auth/2fa/verify", post(auth::verify_2fa))
        // Library config
        .route("/library/config", get(library::get_config))
        .route("/library/config", post(library::update_config))
        // Books
        .route("/books", get(books::list_books))
        .route("/books/search", get(search::search_books))
        .route("/books/tags", get(books::list_tags))
        .route("/chat", post(chat::chat_handler))
        .route("/genie/chat", post(genie::chat))
        .route("/books", post(books::create_book))
        .route(
            "/books/:id",
            get(books::get_book)
                .put(books::update_book)
                .delete(books::delete_book),
        )
        .route("/books/reorder", axum::routing::patch(books::reorder_books))
        // Authors
        .route("/authors", get(author::list_authors))
        .route("/authors", post(author::create_author))
        .route("/authors/:id", get(author::get_author))
        .route("/authors/:id", axum::routing::delete(author::delete_author))
        // Tags
        .route("/tags", get(tag::list_tags))
        .route("/tags", post(tag::create_tag))
        .route("/tags/:id", get(tag::get_tag))
        .route("/tags/:id", axum::routing::delete(tag::delete_tag))
        // Peers
        .route("/peers", get(peer::list_peers))
        .route("/peers/connect", post(peer::connect))
        .route("/peers/incoming", post(peer::receive_connection_request)) // Receive incoming connection
        .route("/peers/push", post(peer::push_operations))
        .route("/peers/pull", get(peer::pull_operations))
        .route("/peers/:id/sync", post(peer::sync_peer)) // Sync remote books by ID
        .route("/peers/sync_by_url", post(peer::sync_peer_by_url)) // Sync by URL (solves Hub ID mismatch)
        .route("/peers/:id/books", get(peer::list_peer_books))
        .route("/peers/books_by_url", post(peer::list_peer_books_by_url)) // Get books by URL
        .route("/peers/search", post(peer::search_local))
        .route("/peers/proxy_search", post(peer::proxy_search))
        .route("/peers/request_by_url", post(peer::request_book_by_url)) // Send request by URL
        .route("/peers/:id/request", post(peer::request_book)) // Send request
        .route("/peers/request", post(peer::receive_request)) // Receive request
        .route("/peers/requests", get(peer::list_requests)) // List incoming requests
        .route(
            "/peers/requests/outgoing",
            get(peer::list_outgoing_requests),
        ) // List outgoing requests
        .route(
            "/peers/requests/outgoing/:id",
            axum::routing::delete(peer::delete_outgoing_request),
        )
        .route("/peers/requests/:id", put(peer::update_request_status)) // Update status
        .route(
            "/peers/requests/:id",
            axum::routing::delete(peer::delete_request),
        ) // Delete request
        // Scanning
        .route("/scan/image", post(scan::scan_image))
        // Batch Operations
        .route("/books/batch/edit", post(batch::batch_edit))
        .route("/books/batch/sort", post(batch::batch_sort))
        .route("/books/duplicates", get(batch::find_duplicates))
        // Copies
        // Copies
        .route("/copies", get(copy::list_copies))
        .route("/copies", post(copy::create_copy))
        .route("/books/:id/copies", get(copy::get_book_copies))
        .route("/copies/:id", axum::routing::delete(copy::delete_copy))
        // Contacts
        .route(
            "/contacts",
            get(contact::list_contacts).post(contact::create_contact),
        )
        .route(
            "/contacts/:id",
            get(contact::get_contact)
                .put(contact::update_contact)
                .delete(contact::delete_contact),
        )
        .route("/profile", put(profile::update_profile))
        // P2P routes
        .route("/loans", get(loan::list_loans).post(loan::create_loan))
        .route("/loans/:id/return", put(loan::return_loan))
        // Lookup
        .route("/lookup/:isbn", get(lookup::lookup_book))
        // Data Import/Export
        .route("/import/file", axum::routing::post(data::import_file))
        // Setup & Config
        .route("/setup", axum::routing::post(setup::setup))
        .route("/reset", axum::routing::post(setup::reset_app))
        .route("/config", get(setup::get_config))
        // Integrations (Professional)
        .route(
            "/integrations/sudoc/search",
            get(integrations::search_sudoc),
        )
        .route(
            "/integrations/osm/libraries",
            get(integrations::search_osm_libraries),
        )
        .route(
            "/integrations/osm/bookstores",
            get(integrations::search_osm_bookstores),
        )
        .route(
            "/integrations/openlibrary/search",
            get(integrations::search_openlibrary),
        )
        .route(
            "/integrations/search_unified",
            get(integrations::search_unified),
        )
        // Gamification
        .route("/user/status", get(gamification::get_user_status))
        // Export
        .route("/export", get(export::export_data))
        .with_state(db)
}
