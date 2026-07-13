//! Collection repository trait and related types

use async_trait::async_trait;

use super::DomainError;

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
    pub book_id: String,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub added_at: String,
    pub is_owned: bool,
    pub digital_formats: Option<Vec<String>>,
    /// The book's personal reading status (`to_read`, `reading`, `read`,
    /// `wanting`, `abandoned`). Drives the "unread = dimmed" rendering of the
    /// series frise.
    pub reading_status: Option<String>,
    /// Reading-order position within a series-typed collection. NULL for
    /// unnumbered members (rendered after the numbered ones).
    pub volume_number: Option<i32>,
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

    /// Set (or clear, with `None`) a collection's `source`. Used to flip a
    /// plain collection to a series (`source = 'series'`) and back.
    async fn set_source(&self, id: &str, source: &str) -> Result<(), DomainError>;

    /// Get all books in a collection, ordered by `volume_number` (numbered
    /// volumes first, ascending; unnumbered last, then by `added_at`).
    async fn get_books(&self, collection_id: &str) -> Result<Vec<CollectionBook>, DomainError>;

    /// Set (or clear, with `None`) the reading-order position of a book within
    /// a collection. No-op if the book is not in the collection.
    async fn set_book_volume(
        &self,
        collection_id: &str,
        book_id: &str,
        volume_number: Option<i32>,
    ) -> Result<(), DomainError>;

    /// Add a book to a collection
    async fn add_book(&self, collection_id: &str, book_id: &str) -> Result<(), DomainError>;

    /// Remove a book from a collection
    async fn remove_book(&self, collection_id: &str, book_id: &str) -> Result<(), DomainError>;

    /// Get all collections a book belongs to
    async fn get_book_collections(&self, book_id: &str) -> Result<Vec<Collection>, DomainError>;

    /// Update which collections a book belongs to (replaces existing associations)
    async fn update_book_collections(
        &self,
        book_id: &str,
        collection_ids: Vec<String>,
    ) -> Result<(), DomainError>;
}
