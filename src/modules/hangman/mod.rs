//! Hangman -- self-contained extension module
//!
//! This module follows the "extension plugin" pattern (ADR-005):
//! all domain types, models, repository, service, and handlers
//! are contained within this folder.
//!
//! Integration points (only 2 lines needed in the rest of the codebase):
//!   - `api/mod.rs`:  .merge(modules::hangman::routes())
//!   - `infrastructure/db.rs`:  modules::hangman::migrate(&db).await?;

pub mod domain;
pub(crate) mod handlers;
mod models;
pub mod repository;
pub mod service;

use axum::{
    Router,
    routing::{get, post},
};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::infrastructure::AppState;

/// Returns the Axum routes for this module
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/game/hangman/difficulties",
            get(handlers::available_difficulties),
        )
        .route("/game/hangman/setup", post(handlers::setup_game))
        .route("/game/hangman/finish", post(handlers::finish_game))
        .route("/game/hangman/scores", get(handlers::get_top_scores))
        .route("/game/hangman/leaderboard", get(handlers::get_leaderboard))
        .route("/game/hangman/public-best", get(handlers::get_public_best))
        .route(
            "/game/hangman/refresh-leaderboard",
            post(handlers::refresh_leaderboard),
        )
}

/// Run database migrations for this module
pub async fn migrate(db: &DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS hangman_scores (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            book_id INTEGER NOT NULL DEFAULT 0,
            difficulty TEXT NOT NULL,
            elapsed_seconds REAL NOT NULL,
            errors INTEGER NOT NULL DEFAULT 0,
            hints_used INTEGER NOT NULL DEFAULT 0,
            won INTEGER NOT NULL DEFAULT 0,
            normalized_score REAL NOT NULL,
            played_at TEXT NOT NULL
        )"
        .to_owned(),
    ))
    .await?;
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_hangman_scores_normalized ON hangman_scores(normalized_score DESC)"
            .to_owned(),
    ))
    .await?;

    // Add book_id column if table was created before this field existed
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE hangman_scores ADD COLUMN book_id INTEGER NOT NULL DEFAULT 0".to_owned(),
        ))
        .await; // Ignore error if column already exists

    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS peer_hangman_scores (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            peer_id INTEGER NOT NULL,
            library_name TEXT NOT NULL,
            best_score REAL NOT NULL,
            difficulty TEXT NOT NULL,
            played_at TEXT NOT NULL,
            synced_at TEXT NOT NULL,
            FOREIGN KEY (peer_id) REFERENCES peers(id) ON DELETE CASCADE
        )"
        .to_owned(),
    ))
    .await?;

    Ok(())
}
