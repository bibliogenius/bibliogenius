//! Domain layer - Pure business abstractions
//!
//! This layer contains NO framework dependencies (no SeaORM, no Axum).
//! Only trait definitions and domain error types.

pub mod errors;
pub mod repositories;

pub use errors::DomainError;
pub use repositories::*;
