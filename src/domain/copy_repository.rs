//! Copy repository trait and related types

use async_trait::async_trait;

use super::DomainError;

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
