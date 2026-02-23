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
