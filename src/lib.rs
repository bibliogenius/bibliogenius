// The account-sync data leg needs a cr-sqlite loader to be linked: either the
// dynamic dev path (`crsqlite`) or the static ship path (`crsqlite-static`).
// Without one, the pool would open with no `crsql_*` functions and `setup_crrs`
// would fail at runtime; fail loud at compile time instead.
#[cfg(all(
    feature = "account_sync",
    not(any(feature = "crsqlite", feature = "crsqlite-static"))
))]
compile_error!(
    "feature \"account_sync\" requires a cr-sqlite loader: enable \"crsqlite\" (dynamic dev) or \"crsqlite-static\" (ship)"
);

pub mod api;
pub mod api_docs;
pub mod crypto;
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
