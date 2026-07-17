// Book note CRUD (book_notes extension module).
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Book Notes (FFI) ────────────────────────────────────────────────

/// FFI-safe book note representation.
pub struct FrbBookNote {
    pub id: i32,
    pub book_id: String,
    pub content: String,
    pub page: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<crate::modules::book_notes::domain::BookNote> for FrbBookNote {
    fn from(n: crate::modules::book_notes::domain::BookNote) -> Self {
        Self {
            id: n.id,
            book_id: n.book_id,
            content: n.content,
            page: n.page,
            created_at: n.created_at,
            updated_at: n.updated_at,
        }
    }
}

/// Get all notes for a book, ordered by creation date (newest first).
pub async fn get_book_notes(book_id: String) -> Result<Vec<FrbBookNote>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::book_notes::repository::SeaOrmBookNoteRepository::new(db.clone());
    use crate::modules::book_notes::domain::BookNoteRepository;
    let notes = repo
        .find_by_book_id(&book_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(notes.into_iter().map(FrbBookNote::from).collect())
}

/// Create a new note for a book.
pub async fn create_book_note(
    book_id: String,
    content: String,
    page: Option<i32>,
) -> Result<FrbBookNote, String> {
    use crate::modules::book_notes::domain::{
        BookNoteRepository, CreateBookNoteInput, MAX_CONTENT_LENGTH,
    };
    if content.trim().is_empty() {
        return Err("Content cannot be empty".to_string());
    }
    if content.len() > MAX_CONTENT_LENGTH {
        return Err(format!("Content exceeds {MAX_CONTENT_LENGTH} characters"));
    }
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::book_notes::repository::SeaOrmBookNoteRepository::new(db.clone());
    let input = CreateBookNoteInput { content, page };
    let note = repo
        .create(&book_id, input)
        .await
        .map_err(|e| e.to_string())?;
    // Log for device sync (payload included for linked-device replication)
    let _ = crate::sync::log_operation(
        db,
        "book_note",
        &note.id.to_string(),
        "INSERT",
        Some(serde_json::json!({
            "book_id": note.book_id,
            "content": note.content,
            "page": note.page,
        })),
    )
    .await;
    Ok(FrbBookNote::from(note))
}

/// Update an existing note.
pub async fn update_book_note(
    id: i32,
    content: String,
    page: Option<i32>,
) -> Result<FrbBookNote, String> {
    use crate::modules::book_notes::domain::{
        BookNoteRepository, MAX_CONTENT_LENGTH, UpdateBookNoteInput,
    };
    if content.trim().is_empty() {
        return Err("Content cannot be empty".to_string());
    }
    if content.len() > MAX_CONTENT_LENGTH {
        return Err(format!("Content exceeds {MAX_CONTENT_LENGTH} characters"));
    }
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::book_notes::repository::SeaOrmBookNoteRepository::new(db.clone());
    let input = UpdateBookNoteInput { content, page };
    let note = repo.update(id, input).await.map_err(|e| e.to_string())?;
    let _ = crate::sync::log_operation(
        db,
        "book_note",
        &id.to_string(),
        "UPDATE",
        Some(serde_json::json!({
            "book_id": note.book_id,
            "content": note.content,
            "page": note.page,
        })),
    )
    .await;
    Ok(FrbBookNote::from(note))
}

/// Delete a note by ID.
pub async fn delete_book_note(id: i32) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::book_notes::repository::SeaOrmBookNoteRepository::new(db.clone());
    use crate::modules::book_notes::domain::BookNoteRepository;
    repo.delete(id).await.map_err(|e| e.to_string())?;
    let _ = crate::sync::log_operation(db, "book_note", &id.to_string(), "DELETE", None).await;
    Ok(())
}
