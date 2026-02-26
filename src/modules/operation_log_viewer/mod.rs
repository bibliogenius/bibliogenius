//! Operation Log Viewer - self-contained read-only module
//!
//! Provides admin endpoints to inspect the operation_log table.
//! No migration needed (reuses existing table).
//!
//! Integration (2 lines):
//!   - `modules/mod.rs`: pub mod operation_log_viewer;
//!   - `api/mod.rs`:     .merge(modules::operation_log_viewer::routes())

pub mod domain;
pub(crate) mod handlers;
pub mod repository;

use axum::{Router, routing::get};

use crate::infrastructure::AppState;

/// Returns the Axum routes for this module
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/operation-log", get(handlers::list_entries))
        .route("/api/admin/operation-log/stats", get(handlers::get_stats))
        .route(
            "/api/admin/operation-log/entity-types",
            get(handlers::get_entity_types),
        )
}
