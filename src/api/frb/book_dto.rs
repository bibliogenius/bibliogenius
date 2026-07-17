// FrbBook DTO and the Model to FrbBook conversion.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ FFI-Compatible Data Structures ============

/// Simplified book structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbBook {
    pub id: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub cover_url: Option<String>,
    pub large_cover_url: Option<String>,
    pub reading_status: Option<String>,
    pub shelf_position: Option<i32>,
    pub user_rating: Option<i32>,
    pub subjects: Option<String>, // JSON array as string
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub finished_reading_at: Option<String>,
    pub started_reading_at: Option<String>,
    pub owned: bool,        // Added for copy management
    pub price: Option<f64>, // Added for bookseller profile
    pub digital_formats: Option<Vec<String>>,
    pub private: bool, // Hidden from network peers
    pub page_count: Option<i32>,
    /// ISO 8601 timestamp of when the book was added to its owner's library
    /// (maps to `books.created_at`). Used by the "new" badge and by the
    /// "recently added" carousel.
    pub added_at: Option<String>,
    /// ISO 8601 timestamp of the last failed hub cover upload for this book.
    /// NULL when the most recent attempt succeeded or none ever ran. Read by
    /// the owner's UI to surface a warning badge while a retry pends.
    pub hub_cover_upload_failed_at: Option<String>,
    /// Whether at least one copy of this book is currently borrowed (from a
    /// peer or a contact), and whether at least one copy the user owns is
    /// currently lent out. Two independent axes: both can be true at once.
    ///
    /// Possession, never reading: `reading_status` keeps its own meaning.
    /// Derived from the `copies` table on read, never stored. `None` means
    /// "not computed" (a write path, a search result), never "false".
    pub is_borrowed: Option<bool>,
    pub is_lent: Option<bool>,
}

/// Convert domain Book to FFI-safe FrbBook
impl From<crate::models::Book> for FrbBook {
    fn from(book: crate::models::Book) -> Self {
        FrbBook {
            id: book.id,
            title: book.title,
            author: book.author,
            isbn: book.isbn,
            summary: book.summary,
            publisher: book.publisher,
            publication_year: book.publication_year,
            cover_url: book.cover_url,
            large_cover_url: book.large_cover_url,
            reading_status: book.reading_status,
            shelf_position: book.shelf_position,
            user_rating: book.user_rating,
            subjects: book
                .subjects
                .map(|s| serde_json::to_string(&s).unwrap_or_default()),
            created_at: None, // Not available in Book DTO
            updated_at: None, // Not available in Book DTO
            finished_reading_at: book.finished_reading_at.flatten(),
            started_reading_at: book.started_reading_at.flatten(),
            owned: book.owned.unwrap_or(true), // Default to owned if None (legacy/missing)
            price: book.price,
            digital_formats: book.digital_formats,
            private: book.private.unwrap_or(false),
            page_count: book.page_count,
            added_at: book.added_at,
            hub_cover_upload_failed_at: book.hub_cover_upload_failed_at,
            is_borrowed: book.is_borrowed,
            is_lent: book.is_lent,
        }
    }
}
