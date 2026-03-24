//! Book Notes - domain types and repository trait
//!
//! Framework-free layer: no SeaORM, no Axum.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::DomainError;

/// Maximum length for note content (in characters).
pub const MAX_CONTENT_LENGTH: usize = 2000;

/// A reading note attached to a book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookNote {
    pub id: i32,
    pub book_id: i32,
    pub content: String,
    pub page: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

/// Input for creating a new note.
#[derive(Debug, Deserialize)]
pub struct CreateBookNoteInput {
    pub content: String,
    pub page: Option<i32>,
}

/// Input for updating an existing note.
#[derive(Debug, Deserialize)]
pub struct UpdateBookNoteInput {
    pub content: String,
    pub page: Option<i32>,
}

#[async_trait]
pub trait BookNoteRepository: Send + Sync {
    /// List all notes for a given book, ordered by created_at DESC.
    async fn find_by_book_id(&self, book_id: i32) -> Result<Vec<BookNote>, DomainError>;

    /// Find a single note by its ID.
    async fn find_by_id(&self, id: i32) -> Result<Option<BookNote>, DomainError>;

    /// Create a new note for a book.
    async fn create(
        &self,
        book_id: i32,
        input: CreateBookNoteInput,
    ) -> Result<BookNote, DomainError>;

    /// Update an existing note.
    async fn update(&self, id: i32, input: UpdateBookNoteInput) -> Result<BookNote, DomainError>;

    /// Delete a note by ID.
    async fn delete(&self, id: i32) -> Result<(), DomainError>;
}
