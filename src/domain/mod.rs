//! Domain layer - Pure business abstractions
//!
//! This layer contains NO framework dependencies (no SeaORM, no Axum).
//! Only trait definitions and domain error types.

pub mod errors;

pub mod author_repository;
pub mod book_repository;
pub mod collection_repository;
pub mod copy_repository;
pub mod gamification_repository;
pub mod linked_device_repository;
pub mod loan_settings_repository;
pub mod notification_repository;

pub use errors::DomainError;

pub use author_repository::*;
pub use book_repository::*;
pub use collection_repository::*;
pub use copy_repository::*;
pub use gamification_repository::*;
pub use linked_device_repository::*;
pub use loan_settings_repository::*;
pub use notification_repository::*;
