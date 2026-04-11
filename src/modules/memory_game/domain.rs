//! Memory Game domain types and repository trait
//!
//! Self-contained domain layer for the memory game module.
//! No framework dependencies (no SeaORM, no Axum).

use async_trait::async_trait;

/// Domain error type (re-exported from main domain for consistency)
pub use crate::domain::DomainError;

/// A book card for the memory game (book with a cover)
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryGameCard {
    pub book_id: i32,
    pub title: String,
    pub cover_url: String,
}

/// Input from a completed memory game
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MemoryGameResult {
    pub difficulty: String,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub pairs_count: i32,
}

/// A persisted memory game score
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryGameScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub pairs_count: i32,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub normalized_score: f64,
    pub played_at: String,
}

/// A peer's memory game score (for leaderboard)
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerMemoryScoreRow {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
}

/// Repository trait for Memory Game
#[async_trait]
pub trait MemoryGameRepository: Send + Sync {
    /// Find all books that have a cover image
    async fn find_books_with_covers(&self) -> Result<Vec<MemoryGameCard>, DomainError>;

    /// Save a game score
    async fn save_score(&self, score: MemoryGameScore) -> Result<MemoryGameScore, DomainError>;

    /// Get top scores ordered by normalized_score DESC
    async fn get_top_scores(&self, limit: u32) -> Result<Vec<MemoryGameScore>, DomainError>;

    /// Get the personal best normalized score
    async fn get_personal_best(&self) -> Result<Option<f64>, DomainError>;

    /// Get the full best score entry (score + difficulty + played_at)
    async fn get_best_score_entry(&self) -> Result<Option<MemoryGameScore>, DomainError>;

    /// Delete all cached peer memory scores for a given peer
    async fn delete_peer_scores(&self, peer_id: i32) -> Result<(), DomainError>;

    /// Upsert a peer's best memory game score (for leaderboard)
    async fn upsert_peer_score(
        &self,
        peer_id: i32,
        library_name: &str,
        best_score: f64,
        difficulty: &str,
        played_at: &str,
    ) -> Result<(), DomainError>;

    /// Get all peer scores for leaderboard display
    async fn get_peer_scores(&self) -> Result<Vec<PeerMemoryScoreRow>, DomainError>;

    /// Delete all local scores (reset personal history)
    async fn delete_all_scores(&self) -> Result<(), DomainError>;
}
