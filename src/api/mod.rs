pub mod admin;
pub mod auth;
pub mod author;
pub mod batch;
pub mod books;
pub mod chat;
pub mod collections;
pub mod contact;
pub mod copy;
pub mod data;
pub mod device;
pub mod discovery;
pub mod e2ee;
pub mod export;
pub mod frb; // FFI API for flutter_rust_bridge
pub mod gamification;
pub mod health;
pub mod hub;
pub mod integrations;
pub mod invite_page;
pub mod library;
pub mod loan;
pub mod lookup;
pub mod peer;
pub mod profile;
pub mod relay;
pub mod sales; // Sales endpoints for bookseller profile
pub mod scan;
pub mod search;
pub mod setup;
pub mod tag;
pub mod user;
pub mod view_counter;

#[cfg(feature = "mcp")]
pub mod mcp;

use axum::{
    Router,
    routing::{delete, get, post, put},
};
use sea_orm::DatabaseConnection;

use crate::infrastructure::AppState;

/// Create API router with a pre-built AppState (returns Router with state applied).
/// Layers the view counter middleware to track peer library consultations.
pub fn api_router_with_state(state: AppState) -> Router {
    let tracker = view_counter::ViewCooldownTracker::new();
    let db_for_views = state.db().clone();

    build_routes()
        .with_state(state)
        // Layer order matters: outermost layer runs first.
        // 1. Extension layers add data to request extensions
        // 2. Middleware reads from extensions and counts views
        .layer(axum::middleware::from_fn(
            view_counter::view_counter_middleware,
        ))
        .layer(axum::Extension(tracker))
        .layer(axum::Extension(db_for_views))
}

/// Create API router from DatabaseConnection (convenience wrapper)
pub fn api_router(db: DatabaseConnection) -> Router {
    let state = AppState::new(db);
    api_router_with_state(state)
}

/// Build the route definitions (internal)
fn build_routes() -> Router<AppState> {
    Router::new()
        // Admin
        .route("/admin/shutdown", post(admin::shutdown))
        // Health check
        .route("/health", get(health::health_check))
        // Auth
        .route("/auth/login", post(auth::login))
        .route("/auth/login-mfa", post(auth::login_mfa))
        .route("/auth/register", post(auth::create_admin))
        .route("/auth/me", get(auth::get_me))
        .route("/auth/2fa/setup", post(auth::setup_2fa))
        .route("/auth/2fa/verify", post(auth::verify_2fa))
        // Pairing (legacy)
        .route("/auth/pairing/code", post(auth::pairing_generate_code))
        .route("/auth/pairing/verify", post(auth::pairing_verify_code))
        // Device pairing and management (ADR-011)
        .route("/devices/pair/offer", post(device::generate_offer))
        .route("/devices/pair/accept", post(device::accept_offer))
        .route("/devices", get(device::list_devices))
        .route("/devices/register", post(device::register_device))
        .route("/devices/:id", delete(device::remove_device))
        // Device sync
        .route(
            "/devices/sync/pending-review",
            get(device::sync_pending_review),
        )
        .route("/devices/sync/approve", post(device::sync_approve))
        .route("/devices/sync/reject", post(device::sync_reject))
        .route("/devices/sync/:id", post(device::trigger_sync))
        // Library config
        .route("/library/config", get(library::get_config))
        .route("/library/config", post(library::update_config))
        // Books
        .route("/books", get(books::list_books))
        .route("/books/search", get(search::search_books))
        .route("/books/tags", get(books::list_tags))
        .route("/chat", post(chat::chat_handler))
        .route("/books", post(books::create_book))
        .route(
            "/books/:id",
            get(books::get_book)
                .put(books::update_book)
                .delete(books::delete_book),
        )
        .route("/books/:id/cover", get(books::get_book_cover))
        .route("/books/reorder", axum::routing::patch(books::reorder_books))
        .route(
            "/books/:id/collections",
            get(collections::get_book_collections).put(collections::update_book_collections),
        )
        // Collections
        .route(
            "/collections",
            get(collections::list_collections).post(collections::create_collection),
        )
        .route(
            "/collections/:id",
            get(collections::get_collection).delete(collections::delete_collection),
        )
        .route(
            "/collections/:id/books",
            get(collections::get_collection_books).post(collections::import_collection),
        )
        .route(
            "/collections/:collection_id/books/:book_id",
            axum::routing::delete(collections::remove_book_from_collection)
                .post(collections::add_book_to_collection),
        )
        // Authors
        .route("/authors", get(author::list_authors))
        .route("/authors", post(author::create_author))
        .route("/authors/:id", get(author::get_author))
        .route("/authors/:id", axum::routing::delete(author::delete_author))
        // Tags
        .route("/tags", get(tag::list_tags))
        .route("/tags", post(tag::create_tag))
        .route("/tags/tree", get(tag::list_tags_tree))
        .route("/tags/:id", get(tag::get_tag))
        .route("/tags/:id", axum::routing::delete(tag::delete_tag))
        // Peers
        .route("/peers", get(peer::list_peers))
        .route("/peers/:id", axum::routing::delete(peer::delete_peer)) // Delete peer
        .route("/peers/:id/status", put(peer::update_peer_status)) // Accept/reject peer
        .route("/peers/:id/url", put(peer::update_peer_url)) // Update peer URL (mDNS IP changes)
        .route(
            "/peers/:id/display-name",
            axum::routing::patch(peer::update_peer_display_name),
        )
        .route(
            "/peers/notify-disconnect",
            post(peer::receive_disconnect_notification),
        )
        .route("/peers/verify-disconnect", post(peer::verify_disconnect))
        .route("/peers/connect", post(peer::connect))
        .route(
            "/peers/auto_approve_all",
            post(peer::auto_approve_all_peers),
        )
        .route("/peers/incoming", post(peer::receive_connection_request)) // Receive incoming connection
        .route("/peers/push", post(peer::push_operations))
        .route("/peers/pull", get(peer::pull_operations))
        .route("/peers/:id/sync", post(peer::sync_peer)) // Sync remote books by ID
        .route("/peers/sync_by_url", post(peer::sync_peer_by_url)) // Sync by URL (solves Hub ID mismatch)
        .route("/peers/:id/cache_books", post(peer::cache_books_by_id)) // Save pre-fetched books to cache
        .route("/peers/:id/books", get(peer::list_peer_books))
        .route("/peers/books_by_url", post(peer::list_peer_books_by_url)) // Get books by URL
        .route(
            "/peers/cached_books_by_url",
            post(peer::get_cached_books_by_url),
        ) // Get cached books with metadata
        .route(
            "/peers/cleanup_stale_cache",
            post(peer::cleanup_stale_peer_books),
        ) // TTL cleanup for privacy
        .route("/peers/search", post(peer::search_local))
        .route("/peers/proxy_search", post(peer::proxy_search))
        .route("/peers/return_book", post(peer::return_borrowed_book)) // Borrower-initiated return
        .route("/peers/request_by_url", post(peer::request_book_by_url)) // Send request by URL
        .route("/peers/:id/request", post(peer::request_book)) // Send request
        .route("/peers/request", post(peer::receive_request)) // Receive request
        .route("/peers/requests", get(peer::list_requests)) // List incoming requests
        .route("/peers/requests/incoming", post(peer::receive_loan_request)) // Receive incoming P2P loan request
        .route(
            "/peers/loans/confirm",
            post(peer::receive_loan_confirmation),
        ) // Receive loan confirmation from lender
        .route(
            "/peers/requests/outgoing",
            get(peer::list_outgoing_requests).post(peer::create_outgoing_request),
        ) // List/Create outgoing requests
        .route(
            "/peers/requests/outgoing/clear",
            axum::routing::delete(peer::clear_outgoing_requests),
        ) // Clear non-pending outgoing requests
        .route(
            "/peers/requests/outgoing/:id",
            axum::routing::delete(peer::delete_outgoing_request),
        )
        .route(
            "/peers/requests/outgoing/sync",
            post(peer::sync_outgoing_requests),
        ) // Sync pending outgoing requests with lenders
        .route(
            "/peers/requests/clear",
            axum::routing::delete(peer::clear_incoming_requests),
        ) // Clear non-pending incoming requests
        .route("/peers/requests/:id", put(peer::update_request_status)) // Update status
        .route(
            "/peers/requests/:id",
            axum::routing::delete(peer::delete_request),
        ) // Delete request
        .route(
            "/peers/requests/cancel/:id",
            axum::routing::delete(peer::cancel_request),
        ) // Receive cancellation notification from peer
        .route(
            "/peers/requests/status/:id",
            put(peer::update_outgoing_status),
        ) // Receive status update notification from lender
        // Local Discovery (mDNS)
        .route("/discovery/local", get(discovery::list_local_peers))
        .route("/discovery/status", get(discovery::mdns_status))
        .route("/discovery/toggle", post(discovery::toggle_mdns))
        // Scanning
        .route("/scan/image", post(scan::scan_image))
        // Batch Operations
        .route("/books/batch/edit", post(batch::batch_edit))
        .route("/books/batch/sort", post(batch::batch_sort))
        .route("/books/duplicates", get(batch::find_duplicates))
        // Copies
        .route("/copies", get(copy::list_copies))
        .route("/copies", post(copy::create_copy))
        .route("/copies/borrowed", get(copy::get_borrowed_copies))
        .route("/books/:id/copies", get(copy::get_book_copies))
        .route(
            "/copies/:id",
            get(copy::get_copy)
                .put(copy::update_copy)
                .delete(copy::delete_copy),
        )
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
        .route(
            "/loan-settings",
            get(loan::get_loan_settings).put(loan::update_loan_settings),
        )
        .route(
            "/loan-settings/effective/:book_id",
            get(loan::get_effective_loan_duration),
        )
        // Sales (Bookseller profile)
        .route("/sales", get(sales::list_sales).post(sales::create_sale))
        .route("/sales/:id", axum::routing::delete(sales::cancel_sale))
        .route("/statistics/sales", get(sales::get_sales_statistics))
        // Lookup
        .route("/lookup/:isbn", get(lookup::lookup_book))
        // Data Import/Export
        .route("/import/file", axum::routing::post(data::import_file))
        // Setup & Config
        .route("/setup", axum::routing::post(setup::setup))
        .route("/reset", axum::routing::post(setup::reset_app))
        .route("/config", get(setup::get_config))
        .route("/identity/init", post(setup::init_identity))
        // Integrations (Professional)
        .route(
            "/integrations/sudoc/search",
            get(integrations::search_sudoc),
        )
        .route(
            "/integrations/openlibrary/search",
            get(integrations::search_openlibrary),
        )
        .route(
            "/integrations/search_unified",
            get(integrations::search_unified),
        )
        .route("/integrations/mcp-config", get(integrations::mcp_config))
        // Gamification
        .route("/user/status", get(gamification::get_user_status))
        .route(
            "/gamification/public-stats",
            get(gamification::get_public_stats),
        )
        .route(
            "/gamification/leaderboard",
            get(gamification::get_leaderboard),
        )
        .route(
            "/gamification/refresh-leaderboard",
            post(gamification::refresh_leaderboard),
        )
        // Book Notes (self-contained module)
        .merge(crate::modules::book_notes::routes())
        // Memory Game (self-contained module)
        .merge(crate::modules::memory_game::routes())
        // Sliding Puzzle (self-contained module)
        .merge(crate::modules::sliding_puzzle::routes())
        // Hangman (self-contained module)
        .merge(crate::modules::hangman::routes())
        // Operation Log Viewer (self-contained module)
        .merge(crate::modules::operation_log_viewer::routes())
        // Peer relay setup
        .route("/peers/relay/setup", post(peer::setup_relay))
        .route(
            "/peers/relay/config",
            get(peer::get_relay_config_endpoint).delete(peer::delete_relay_config_endpoint),
        )
        // Peer relay library sync (ADR-012)
        .route(
            "/peers/relay/library_request",
            post(peer::relay_library_request),
        )
        .route(
            "/peers/relay/await_response",
            post(peer::await_relay_response),
        )
        // Relay hub endpoints (any instance can serve as a relay)
        .route("/relay/poll_now", post(relay::poll_now))
        .route("/relay/status", get(relay::relay_status))
        .route("/relay/mailbox", post(relay::create_mailbox))
        .route(
            "/relay/mailbox/:uuid/messages",
            post(relay::deposit_message).get(relay::collect_messages),
        )
        .route(
            "/relay/mailbox/:uuid/messages/:id",
            axum::routing::delete(relay::ack_message),
        )
        // E2EE encrypted peer messages
        .route("/e2ee/message", post(e2ee::receive_encrypted_message))
        // View stats
        .route("/stats/views", get(view_counter::get_view_stats_handler))
        // Export/Import
        .route("/export", get(export::export_data))
        .route("/import", post(export::import_data))
        .route("/import-upsert", post(export::import_data_upsert))
}
