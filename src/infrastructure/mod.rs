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
pub mod config;
pub mod db;
pub mod repositories;
pub mod seed;
pub mod server;
pub mod state;

pub use repositories::*;
pub use state::AppState;
