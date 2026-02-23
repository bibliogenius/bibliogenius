//! Author repository trait and related types

use async_trait::async_trait;

use super::DomainError;

/// Author data for API responses
#[derive(Debug, Clone, serde::Serialize)]
pub struct Author {
    pub id: i32,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Repository trait for Author entity
#[async_trait]
pub trait AuthorRepository: Send + Sync {
    /// Find all authors
    async fn find_all(&self) -> Result<Vec<Author>, DomainError>;

    /// Find an author by ID
    async fn find_by_id(&self, id: i32) -> Result<Option<Author>, DomainError>;

    /// Create a new author
    async fn create(&self, name: String) -> Result<Author, DomainError>;

    /// Delete an author by ID
    async fn delete(&self, id: i32) -> Result<(), DomainError>;
}
