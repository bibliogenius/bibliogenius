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

/// Copy data for API responses
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Copy {
    pub id: Option<i32>,
    pub book_id: i32,
    pub library_id: i32,
    pub acquisition_date: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    pub is_temporary: bool,
    pub book_title: Option<String>,
    pub book_cover: Option<String>,
    pub price: Option<f64>,
    pub sold_at: Option<String>,
}

/// Paginated copies result
#[derive(Debug)]
pub struct PaginatedCopies {
    pub copies: Vec<Copy>,
    pub total: usize,
}

/// Input for creating a copy
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CreateCopyInput {
    pub book_id: i32,
    pub library_id: i32,
    pub acquisition_date: Option<String>,
    pub notes: Option<String>,
    pub status: String,
    pub is_temporary: bool,
    pub price: Option<f64>,
}

/// Input for updating a copy
#[derive(Debug, Clone, serde::Deserialize)]
pub struct UpdateCopyInput {
    pub status: Option<String>,
    pub notes: Option<Option<String>>,
    pub acquisition_date: Option<Option<String>>,
    pub price: Option<Option<f64>>,
}

/// Repository trait for Copy entity
#[async_trait]
pub trait CopyRepository: Send + Sync {
    /// Find all copies with book titles
    async fn find_all(&self) -> Result<PaginatedCopies, DomainError>;

    /// Find a copy by ID
    async fn find_by_id(&self, id: i32) -> Result<Option<Copy>, DomainError>;

    /// Find copies for a specific book
    async fn find_by_book_id(&self, book_id: i32) -> Result<PaginatedCopies, DomainError>;

    /// Find borrowed copies (is_temporary=true) with book details
    async fn find_borrowed(&self) -> Result<PaginatedCopies, DomainError>;

    /// Create a new copy
    async fn create(&self, input: CreateCopyInput) -> Result<Copy, DomainError>;

    /// Update a copy
    async fn update(&self, id: i32, input: UpdateCopyInput) -> Result<Copy, DomainError>;

    /// Delete a copy
    async fn delete(&self, id: i32) -> Result<(), DomainError>;
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

/// Collection data for API responses
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    pub total_books: i64,
    pub owned_books: i64,
}

/// Book data within a collection
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CollectionBook {
    pub book_id: i32,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub added_at: String,
    pub is_owned: bool,
    pub digital_formats: Option<Vec<String>>,
}

/// Input for creating a collection
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CreateCollectionInput {
    pub name: String,
    pub description: Option<String>,
    pub source: Option<String>,
}

/// Repository trait for Collection entity
#[async_trait]
pub trait CollectionRepository: Send + Sync {
    /// Find all collections with book counts
    async fn find_all(&self) -> Result<Vec<Collection>, DomainError>;

    /// Find a collection by ID
    async fn find_by_id(&self, id: &str) -> Result<Option<Collection>, DomainError>;

    /// Create a new collection
    async fn create(&self, input: CreateCollectionInput) -> Result<Collection, DomainError>;

    /// Delete a collection by ID
    async fn delete(&self, id: &str) -> Result<(), DomainError>;

    /// Get all books in a collection
    async fn get_books(&self, collection_id: &str) -> Result<Vec<CollectionBook>, DomainError>;

    /// Add a book to a collection
    async fn add_book(&self, collection_id: &str, book_id: i32) -> Result<(), DomainError>;

    /// Remove a book from a collection
    async fn remove_book(&self, collection_id: &str, book_id: i32) -> Result<(), DomainError>;

    /// Get all collections a book belongs to
    async fn get_book_collections(&self, book_id: i32) -> Result<Vec<Collection>, DomainError>;

    /// Update which collections a book belongs to (replaces existing associations)
    async fn update_book_collections(
        &self,
        book_id: i32,
        collection_ids: Vec<String>,
    ) -> Result<(), DomainError>;
}
