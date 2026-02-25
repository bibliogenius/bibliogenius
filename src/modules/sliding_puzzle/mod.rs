//! Sliding Puzzle - self-contained extension module
//!
//! This module follows the "extension plugin" pattern (ADR-005):
//! all domain types, models, repository, service, and handlers
//! are contained within this folder.
//!
//! Integration points (only 2-3 lines needed in the rest of the codebase):
//!   - `modules/mod.rs`:  pub mod sliding_puzzle;
//!   - `api/mod.rs`:  .merge(modules::sliding_puzzle::routes())
//!   - `infrastructure/db.rs`:  modules::sliding_puzzle::migrate(&db).await?;

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
            "/game/puzzle/difficulties",
            get(handlers::available_difficulties),
        )
        .route("/game/puzzle/setup", post(handlers::setup_game))
        .route("/game/puzzle/finish", post(handlers::finish_game))
        .route("/game/puzzle/scores", get(handlers::get_top_scores))
        .route("/game/puzzle/public-best", get(handlers::get_public_best))
        .route("/game/puzzle/leaderboard", get(handlers::get_leaderboard))
        .route(
            "/game/puzzle/refresh-leaderboard",
            post(handlers::refresh_leaderboard),
        )
}

/// Run database migrations for this module
pub async fn migrate(db: &DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    // Migration 047: Sliding puzzle scores
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS sliding_puzzle_scores (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            difficulty TEXT NOT NULL,
            grid_size INTEGER NOT NULL,
            elapsed_seconds REAL NOT NULL,
            move_count INTEGER NOT NULL,
            par_moves INTEGER NOT NULL,
            normalized_score REAL NOT NULL,
            played_at TEXT NOT NULL
        )"
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_puzzle_scores_normalized ON sliding_puzzle_scores(normalized_score DESC)"
            .to_owned(),
    ))
    .await?;

    // Migration 048: Peer puzzle scores (leaderboard cache)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE TABLE IF NOT EXISTS peer_puzzle_scores (
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
