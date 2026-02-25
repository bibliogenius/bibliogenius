//! Memory Game service — business logic
//!
//! Handles difficulty configuration, card selection, scoring formula,
//! and game lifecycle. All DB access goes through MemoryGameRepository trait.

use std::collections::HashSet;

use chrono::Local;
use rand::seq::SliceRandom;
use rand::thread_rng;

use super::domain::{
    DomainError, MemoryGameCard, MemoryGameRepository, MemoryGameResult, MemoryGameScore,
};

/// Difficulty levels for the memory game
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryDifficulty {
    Easy,
    Medium,
    Hard,
    Expert,
    Master,
}

impl MemoryDifficulty {
    /// Number of pairs to find
    pub fn pairs_count(&self) -> usize {
        match self {
            Self::Easy => 3,
            Self::Medium => 6,
            Self::Hard => 8,
            Self::Expert => 10,
            Self::Master => 15,
        }
    }

    /// Grid dimensions (columns, rows)
    pub fn grid_dimensions(&self) -> (u8, u8) {
        match self {
            Self::Easy => (3, 2),
            Self::Medium => (3, 4),
            Self::Hard => (4, 4),
            Self::Expert => (5, 4),
            Self::Master => (5, 6),
        }
    }

    /// Score multiplier
    pub fn multiplier(&self) -> f64 {
        match self {
            Self::Easy => 1.0,
            Self::Medium => 1.5,
            Self::Hard => 2.0,
            Self::Expert => 2.5,
            Self::Master => 3.0,
        }
    }

    /// Maximum allowed time in seconds
    pub fn max_time_seconds(&self) -> f64 {
        match self {
            Self::Easy => 60.0,
            Self::Medium => 120.0,
            Self::Hard => 180.0,
            Self::Expert => 240.0,
            Self::Master => 360.0,
        }
    }

    /// Minimum number of books with covers required for this difficulty
    pub fn min_books_required(&self) -> usize {
        self.pairs_count()
    }

    /// All difficulty levels in order
    pub fn all() -> &'static [MemoryDifficulty] {
        &[
            Self::Easy,
            Self::Medium,
            Self::Hard,
            Self::Expert,
            Self::Master,
        ]
    }

    /// Parse from string (case-insensitive)
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        match s.to_lowercase().as_str() {
            "easy" => Ok(Self::Easy),
            "medium" => Ok(Self::Medium),
            "hard" => Ok(Self::Hard),
            "expert" => Ok(Self::Expert),
            "master" => Ok(Self::Master),
            _ => Err(DomainError::Validation(format!(
                "Unknown difficulty: {}",
                s
            ))),
        }
    }

    /// Convert to string
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Easy => "easy",
            Self::Medium => "medium",
            Self::Hard => "hard",
            Self::Expert => "expert",
            Self::Master => "master",
        }
    }
}

/// Get available difficulties based on how many books have covers
pub async fn available_difficulties(
    repo: &dyn MemoryGameRepository,
) -> Result<Vec<MemoryDifficulty>, DomainError> {
    let books = repo.find_books_with_covers().await?;

    // Count only books with distinct covers to match setup_game dedup logic
    let mut seen = HashSet::new();
    let count = books
        .into_iter()
        .filter(|b| seen.insert(b.cover_url.clone()))
        .count();

    let available = MemoryDifficulty::all()
        .iter()
        .filter(|d| count >= d.min_books_required())
        .copied()
        .collect();

    Ok(available)
}

/// Set up a new game: pick random books, duplicate into pairs, shuffle
pub async fn setup_game(
    repo: &dyn MemoryGameRepository,
    difficulty: MemoryDifficulty,
) -> Result<Vec<MemoryGameCard>, DomainError> {
    let books = repo.find_books_with_covers().await?;
    let pairs = difficulty.pairs_count();

    // Deduplicate by cover_url so two books sharing the same cover
    // never appear as separate pairs on the board.
    let mut seen_covers = HashSet::new();
    let mut unique_books: Vec<MemoryGameCard> = books
        .into_iter()
        .filter(|b| seen_covers.insert(b.cover_url.clone()))
        .collect();

    if unique_books.len() < pairs {
        return Err(DomainError::Validation(format!(
            "Not enough books with distinct covers: need {}, have {}",
            pairs,
            unique_books.len()
        )));
    }

    let mut rng = thread_rng();
    unique_books.shuffle(&mut rng);
    let selected: Vec<MemoryGameCard> = unique_books.into_iter().take(pairs).collect();

    let mut cards: Vec<MemoryGameCard> = Vec::with_capacity(pairs * 2);
    for card in &selected {
        cards.push(card.clone());
        cards.push(card.clone());
    }
    cards.shuffle(&mut rng);

    Ok(cards)
}

/// Compute the normalized score for a completed game
///
/// Formula:
///   time_score = max(0, (max_time - elapsed) / max_time) * 1000
///   error_penalty = errors * 50
///   raw_score = max(0, time_score - error_penalty)
///   normalized_score = raw_score * difficulty_multiplier
pub fn compute_score(result: &MemoryGameResult) -> Result<f64, DomainError> {
    let difficulty = MemoryDifficulty::parse(&result.difficulty)?;
    let max_time = difficulty.max_time_seconds();

    let time_score = ((max_time - result.elapsed_seconds) / max_time * 1000.0).max(0.0);
    let error_penalty = result.errors as f64 * 50.0;
    let raw_score = (time_score - error_penalty).max(0.0);
    let normalized_score = raw_score * difficulty.multiplier();

    Ok(normalized_score)
}

/// Finish a game: compute score, persist it, return the saved score
pub async fn finish_game(
    repo: &dyn MemoryGameRepository,
    result: MemoryGameResult,
) -> Result<MemoryGameScore, DomainError> {
    let normalized_score = compute_score(&result)?;
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let score = MemoryGameScore {
        id: None,
        difficulty: result.difficulty,
        pairs_count: result.pairs_count,
        elapsed_seconds: result.elapsed_seconds,
        errors: result.errors,
        normalized_score,
        played_at: now,
    };

    repo.save_score(score).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(difficulty: &str, elapsed: f64, errors: i32, pairs: i32) -> MemoryGameResult {
        MemoryGameResult {
            difficulty: difficulty.to_string(),
            elapsed_seconds: elapsed,
            errors,
            pairs_count: pairs,
        }
    }

    #[test]
    fn test_compute_score_perfect_easy() {
        let result = make_result("easy", 0.0, 0, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_perfect_master() {
        let result = make_result("master", 0.0, 0, 15);
        let score = compute_score(&result).unwrap();
        assert!((score - 3000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_with_errors() {
        let result = make_result("easy", 0.0, 3, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 850.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_over_time() {
        let result = make_result("easy", 60.0, 0, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_way_over_time() {
        let result = make_result("easy", 120.0, 0, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_half_time_no_errors() {
        let result = make_result("easy", 30.0, 0, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_half_time_medium() {
        let result = make_result("medium", 60.0, 0, 6);
        let score = compute_score(&result).unwrap();
        assert!((score - 750.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_many_errors_clamps_to_zero() {
        let result = make_result("easy", 0.0, 25, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_invalid_difficulty() {
        let result = make_result("impossible", 0.0, 0, 3);
        assert!(compute_score(&result).is_err());
    }

    #[test]
    fn test_difficulty_parse() {
        assert_eq!(
            MemoryDifficulty::parse("easy").unwrap(),
            MemoryDifficulty::Easy
        );
        assert_eq!(
            MemoryDifficulty::parse("MASTER").unwrap(),
            MemoryDifficulty::Master
        );
        assert!(MemoryDifficulty::parse("unknown").is_err());
    }

    #[test]
    fn test_difficulty_pairs_count() {
        assert_eq!(MemoryDifficulty::Easy.pairs_count(), 3);
        assert_eq!(MemoryDifficulty::Medium.pairs_count(), 6);
        assert_eq!(MemoryDifficulty::Hard.pairs_count(), 8);
        assert_eq!(MemoryDifficulty::Expert.pairs_count(), 10);
        assert_eq!(MemoryDifficulty::Master.pairs_count(), 15);
    }

    #[test]
    fn test_difficulty_grid_dimensions() {
        let (cols, rows) = MemoryDifficulty::Easy.grid_dimensions();
        assert_eq!(cols * rows, 6);

        let (cols, rows) = MemoryDifficulty::Master.grid_dimensions();
        assert_eq!(cols * rows, 30);
    }
}
