//! Domain error types
//!
//! These errors are framework-agnostic and represent business-level failures.

use std::fmt;

#[derive(Debug)]
pub enum DomainError {
    /// Resource not found
    NotFound,
    /// Validation error with message
    Validation(String),
    /// Database/persistence error
    Database(String),
    /// External service error
    External(String),
    /// Generic internal error
    Internal(String),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DomainError::NotFound => write!(f, "Resource not found"),
            DomainError::Validation(msg) => write!(f, "Validation error: {}", msg),
            DomainError::Database(msg) => write!(f, "Database error: {}", msg),
            DomainError::External(msg) => write!(f, "External service error: {}", msg),
            DomainError::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for DomainError {}

// Conversion from SeaORM errors (used in infrastructure layer)
impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        DomainError::Database(e.to_string())
    }
}
