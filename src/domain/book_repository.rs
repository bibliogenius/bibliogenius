//! Book repository trait and related types

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
    /// Peer-catalogue filter. When true, restrict to books the user owns AND
    /// that are not `private`.
    ///
    /// This is NOT a plain ownership predicate: it exists to build the view a
    /// LAN peer is allowed to see, so it silently excludes private books.
    /// Owner-facing callers want [`Self::owned`] instead, or they will hide the
    /// owner's private books from the owner (ADR-048).
    pub owned_only: Option<bool>,
    /// Owner-facing ownership predicate. `Some(true)` keeps owned books,
    /// `Some(false)` keeps the wishlist, `None` keeps both. Never touches
    /// `private`. See [`Self::owned_only`] for the peer-facing variant.
    pub owned: Option<bool>,
    /// Restrict to books belonging to a collection, identified by its uuid or,
    /// failing that, by its exact name (case-insensitive).
    pub collection: Option<String>,
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
    async fn find_by_id(&self, id: &str) -> Result<Option<Book>, DomainError>;

    /// Find a single book by ISBN.
    ///
    /// `books.isbn` carries no UNIQUE constraint and nothing deduplicates on
    /// insert, so several rows may share an ISBN. The oldest (lowest
    /// `created_at`) is returned, which makes repeated lookups agree with each
    /// other. Callers needing every match filter on ISBN through
    /// [`BookRepository::find_all`] instead.
    async fn find_by_isbn(&self, isbn: &str) -> Result<Option<Book>, DomainError>;

    /// Create a new book
    async fn create(&self, book: Book) -> Result<Book, DomainError>;

    /// Update an existing book
    async fn update(&self, id: &str, book: Book) -> Result<Book, DomainError>;

    /// Delete a book by ID
    async fn delete(&self, id: &str) -> Result<(), DomainError>;

    /// Find books that have an ISBN but no persisted cover URL.
    /// Returns (book_id, isbn) pairs.
    async fn find_missing_covers(&self) -> Result<Vec<(String, String)>, DomainError>;

    /// Update only the cover_url field for a single book (lightweight, no full reload).
    async fn update_cover_url(&self, id: &str, cover_url: &str) -> Result<(), DomainError>;
}
