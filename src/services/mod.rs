//! Services Layer
//!
//! This module contains pure business logic extracted from HTTP handlers.
//! Services can be called directly via FFI or through Axum handlers.

pub mod book_service;
pub mod contact_service;
pub mod loan_service;

// Re-export for convenience
pub use book_service::*;
