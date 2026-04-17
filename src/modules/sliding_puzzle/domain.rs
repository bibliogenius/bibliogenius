//! Sliding Puzzle domain types and repository trait
//!
//! Self-contained domain layer for the sliding puzzle module.
//! No framework dependencies (no SeaORM, no Axum).

use async_trait::async_trait;

/// Domain error type (re-exported from main domain for consistency)
pub use crate::domain::DomainError;

/// A book selected for the puzzle (must have a cover image)
#[derive(Debug, Clone, serde::Serialize)]
pub struct PuzzleBook {
    pub book_id: i32,
    pub title: String,
    pub cover_url: String,
}

/// A generated puzzle board ready to play
#[derive(Debug, Clone, serde::Serialize)]
pub struct PuzzleBoard {
    pub book_id: i32,
    pub title: String,
    pub cover_url: String,
    pub grid_size: u8,
    pub tiles: Vec<u8>,
    pub empty_index: usize,
    pub par_moves: u32,
}

/// Input from a completed sliding puzzle game
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PuzzleResult {
    pub difficulty: String,
    pub grid_size: u8,
    pub elapsed_seconds: f64,
    pub move_count: u32,
    pub par_moves: u32,
}

/// A persisted sliding puzzle score
#[derive(Debug, Clone, serde::Serialize)]
pub struct PuzzleScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub grid_size: i32,
    pub elapsed_seconds: f64,
    pub move_count: i32,
    pub par_moves: i32,
    pub normalized_score: f64,
    pub played_at: String,
}

/// A peer's sliding puzzle score (for leaderboard)
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerPuzzleScoreRow {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
}

/// Repository trait for Sliding Puzzle
#[async_trait]
pub trait SlidingPuzzleRepository: Send + Sync {
    /// Find all books that have a cover image
    async fn find_books_with_covers(&self) -> Result<Vec<PuzzleBook>, DomainError>;

    /// Save a game score
    async fn save_score(&self, score: PuzzleScore) -> Result<PuzzleScore, DomainError>;

    /// Get top scores ordered by normalized_score DESC
    async fn get_top_scores(&self, limit: u32) -> Result<Vec<PuzzleScore>, DomainError>;

    /// Get the personal best normalized score
    async fn get_personal_best(&self) -> Result<Option<f64>, DomainError>;

    /// Get the full best score entry (for public API)
    async fn get_best_score_entry(&self) -> Result<Option<PuzzleScore>, DomainError>;

    /// One best entry per difficulty actually played — used by the public
    /// stats bundle so peers see all difficulties, not just the overall best.
    async fn get_best_score_entries_per_difficulty(&self) -> Result<Vec<PuzzleScore>, DomainError>;

    /// Delete all cached peer puzzle scores for a given peer
    async fn delete_peer_scores(&self, peer_id: i32) -> Result<(), DomainError>;

    /// Upsert a peer's best puzzle score (for leaderboard)
    async fn upsert_peer_score(
        &self,
        peer_id: i32,
        library_name: &str,
        best_score: f64,
        difficulty: &str,
        played_at: &str,
    ) -> Result<(), DomainError>;

    /// Get all peer scores for leaderboard display
    async fn get_peer_scores(&self) -> Result<Vec<PeerPuzzleScoreRow>, DomainError>;

    /// Delete all local scores (reset personal history)
    async fn delete_all_scores(&self) -> Result<(), DomainError>;
}
