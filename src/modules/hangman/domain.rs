//! Hangman domain types and repository trait
//!
//! Self-contained domain layer for the hangman module.
//! No framework dependencies (no SeaORM, no Axum).

use async_trait::async_trait;

/// Domain error type (re-exported from main domain for consistency)
pub use crate::domain::DomainError;

/// A book eligible for the hangman game (title + hint data)
#[derive(Debug, Clone, serde::Serialize)]
pub struct HangmanBook {
    pub book_id: i32,
    pub title: String,
    pub author: String,
    pub cover_url: Option<String>,
}

/// A single character in the hangman display
#[derive(Debug, Clone, serde::Serialize)]
pub struct HangmanChar {
    /// Original character (with accent)
    pub character: char,
    /// Normalized base character (lowercase, no accent)
    pub base_char: char,
    /// Whether this character has been revealed
    pub revealed: bool,
    /// Whether this character must be guessed (true for letters and digits)
    pub is_guessable: bool,
}

/// Game setup returned to the client
#[derive(Debug, Clone, serde::Serialize)]
pub struct HangmanSetup {
    pub book_id: i32,
    pub title: String,
    pub display: Vec<HangmanChar>,
    pub author: String,
    pub cover_url: Option<String>,
    pub max_errors: u8,
    pub hints_available: u8,
    pub difficulty: String,
}

/// Input from a completed hangman game
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HangmanResult {
    pub book_id: i32,
    pub difficulty: String,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub hints_used: i32,
    pub won: bool,
}

/// A persisted hangman score
#[derive(Debug, Clone, serde::Serialize)]
pub struct HangmanScore {
    pub id: Option<i32>,
    pub book_id: i32,
    pub difficulty: String,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub hints_used: i32,
    pub won: bool,
    pub normalized_score: f64,
    pub played_at: String,
}

/// A peer's hangman score (for leaderboard)
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerHangmanScoreRow {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
}

/// Repository trait for Hangman
#[async_trait]
pub trait HangmanRepository: Send + Sync {
    /// Find all books eligible for hangman (with title + author)
    async fn find_eligible_books(&self) -> Result<Vec<HangmanBook>, DomainError>;

    /// Get book IDs from the N most recent games (for anti-repeat filtering)
    async fn get_recent_book_ids(&self, limit: u32) -> Result<Vec<i32>, DomainError>;

    /// Save a game score
    async fn save_score(&self, score: HangmanScore) -> Result<HangmanScore, DomainError>;

    /// Get top scores ordered by normalized_score DESC
    async fn get_top_scores(&self, limit: u32) -> Result<Vec<HangmanScore>, DomainError>;

    /// Get the personal best normalized score
    async fn get_personal_best(&self) -> Result<Option<f64>, DomainError>;

    /// Get the full best score entry (score + difficulty + played_at)
    async fn get_best_score_entry(&self) -> Result<Option<HangmanScore>, DomainError>;

    /// Delete all cached peer hangman scores for a given peer
    async fn delete_peer_scores(&self, peer_id: i32) -> Result<(), DomainError>;

    /// Upsert a peer's best hangman score (for leaderboard)
    async fn upsert_peer_score(
        &self,
        peer_id: i32,
        library_name: &str,
        best_score: f64,
        difficulty: &str,
        played_at: &str,
    ) -> Result<(), DomainError>;

    /// Get all peer scores for leaderboard display
    async fn get_peer_scores(&self) -> Result<Vec<PeerHangmanScoreRow>, DomainError>;
}
