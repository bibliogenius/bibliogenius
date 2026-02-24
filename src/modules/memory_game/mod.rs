//! Memory Game — self-contained extension module
//!
//! This module follows the "extension plugin" pattern:
//! all domain types, models, repository, service, and handlers
//! are contained within this folder.
//!
//! Integration points (only 2 lines needed in the rest of the codebase):
//!   - `api/mod.rs`:  .merge(modules::memory_game::routes())
//!   - `infrastructure/db.rs`:  modules::memory_game::migrate(&db).await?;

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
            "/game/memory/difficulties",
            get(handlers::available_difficulties),
        )
        .route("/game/memory/setup", post(handlers::setup_game))
        .route("/game/memory/finish", post(handlers::finish_game))
        .route("/game/memory/scores", get(handlers::get_top_scores))
        .route("/game/memory/leaderboard", get(handlers::get_leaderboard))
        .route("/game/memory/public-best", get(handlers::get_public_best))
        .route(
            "/game/memory/refresh-leaderboard",
            post(handlers::refresh_leaderboard),
        )
}

/// Run database migrations for this module
pub async fn migrate(db: &DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    // Migration 045: Memory game scores
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS memory_game_scores (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            difficulty TEXT NOT NULL,
            pairs_count INTEGER NOT NULL,
            elapsed_seconds REAL NOT NULL,
            errors INTEGER NOT NULL DEFAULT 0,
            normalized_score REAL NOT NULL,
            played_at TEXT NOT NULL
        )"
        .to_owned(),
    ))
    .await?;
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_memory_scores_normalized ON memory_game_scores(normalized_score DESC)"
            .to_owned(),
    ))
    .await?;

    // Migration 046: Peer memory scores (leaderboard cache)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS peer_memory_scores (
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
