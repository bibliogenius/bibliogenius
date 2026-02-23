//! Domain layer - Pure business abstractions
//!
//! This layer contains NO framework dependencies (no SeaORM, no Axum).
//! Only trait definitions and domain error types.

pub mod errors;

pub mod author_repository;
pub mod book_repository;
pub mod collection_repository;
pub mod copy_repository;
pub mod memory_game_repository;

pub use errors::DomainError;

pub use author_repository::*;
pub use book_repository::*;
pub use collection_repository::*;
pub use copy_repository::*;
pub use memory_game_repository::*;
