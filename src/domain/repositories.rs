//! Repository trait definitions
//!
//! These traits define the contract for data access.
//! Implementations live in the infrastructure layer.

use async_trait::async_trait;

use super::DomainError;
use crate::models::book::Book;

/// Filter criteria for book queries
#[derive(Debug, Default, Clone)]
pub struct BookFilter {
    pub status: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub tag: Option<String>,
    pub query: Option<String>,
    pub sort: Option<String>,
    pub page: Option<u64>,
    pub limit: Option<u64>,
}

/// Paginated result with total count
#[derive(Debug)]
pub struct PaginatedBooks {
    pub books: Vec<Book>,
    pub total: u64,
}

/// Repository trait for Book entity
#[async_trait]
pub trait BookRepository: Send + Sync {
    /// Find all books matching the filter criteria with pagination support
    async fn find_all(&self, filter: BookFilter) -> Result<PaginatedBooks, DomainError>;

    /// Find a single book by ID
    async fn find_by_id(&self, id: i32) -> Result<Option<Book>, DomainError>;

    /// Create a new book
    async fn create(&self, book: Book) -> Result<Book, DomainError>;

    /// Update an existing book
    async fn update(&self, id: i32, book: Book) -> Result<Book, DomainError>;

    /// Delete a book by ID
    async fn delete(&self, id: i32) -> Result<(), DomainError>;
}
