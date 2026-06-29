//! Infrastructure layer - Framework implementations
//!
//! This layer contains:
//! - Database connection and migrations (db)
//! - HTTP server setup (server)
//! - Configuration loading (config)
//! - Authentication (auth)
//! - Repository implementations (repositories)
//! - Application state (state)

pub mod auth;
pub mod book_local;
pub mod config;
#[cfg(any(feature = "crsqlite", feature = "crsqlite-static"))]
pub mod crsqlite_crr;
#[cfg(feature = "crsqlite-static")]
pub mod crsqlite_static;
pub mod db;
pub mod nonce_store;
pub mod referential_integrity;
pub mod repositories;
pub mod seed;
pub mod server;
pub mod state;
pub mod uuid_lookup;

pub use repositories::*;
pub use state::AppState;
