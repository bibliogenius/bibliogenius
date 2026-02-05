pub mod api;
pub mod api_docs;
pub mod domain;
mod frb_generated; /* AUTO INJECTED BY flutter_rust_bridge. This line may not be accurate, and you can change it according to your needs. */
pub mod infrastructure;
pub mod models;
pub mod modules;
pub mod services;
pub mod sync;
pub mod utils;

// Re-exports for backward compatibility during migration
// TODO: Update all imports and remove these re-exports
pub use infrastructure::auth;
pub use infrastructure::config;
pub use infrastructure::db;
pub use infrastructure::seed;
pub use infrastructure::server;
pub use modules::import;
pub use modules::integrations::google_books;
pub use modules::integrations::inventaire as inventaire_client;
pub use modules::integrations::openlibrary;
