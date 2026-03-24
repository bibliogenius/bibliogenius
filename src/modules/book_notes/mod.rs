//! Book Notes - self-contained extension module
//!
//! Allows users to attach multiple timestamped reading notes to a book.
//!
//! This module follows the "extension plugin" pattern (ADR-005):
//! all domain types, models, repository, and handlers are contained
//! within this folder.
//!
//! Integration points (only 2 lines needed in the rest of the codebase):
//!   - `api/mod.rs`:  .merge(modules::book_notes::routes())
//!   - `infrastructure/db.rs`:  modules::book_notes::migrate(&db).await?;

pub mod domain;
pub(crate) mod handlers;
pub mod models;
pub mod repository;

use axum::{
    Router,
    routing::{delete, get, post, put},
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::infrastructure::AppState;

/// Returns the Axum routes for this module.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/books/:book_id/notes", get(handlers::list_notes))
        .route("/books/:book_id/notes", post(handlers::create_note))
        .route("/book-notes/:id", put(handlers::update_note))
        .route("/book-notes/:id", delete(handlers::delete_note))
}

/// Run database migrations for this module.
pub async fn migrate(db: &DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS book_notes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            book_id INTEGER NOT NULL,
            content TEXT NOT NULL,
            page INTEGER,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE
        )"
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_book_notes_book_id ON book_notes(book_id, created_at DESC)"
            .to_owned(),
    ))
    .await?;

    Ok(())
}
