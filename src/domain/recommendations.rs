//! Pure types for the local reading recommendations engine (ADR-059).
//!
//! Everything here is computed on-device from the user's own library: no
//! cloud, no profiling, no data leaves the device. Scoring lives in
//! `services/recommendation_service.rs`; these types are the contract it
//! shares with the API and FFI layers.

use crate::models::book::Book;

/// A lightweight representation of the user's reading preferences, computed
/// from the books they read, rated or shelved as favorites.
#[derive(Debug, Clone, Default)]
pub struct TasteProfile {
    /// Top shelves/genres by frequency among liked books, most frequent
    /// first. Labels keep their original casing for display; matching is
    /// done on the normalized (trimmed, lowercased) form.
    pub top_subjects: Vec<(String, u32)>,
    /// Authors of liked books, with the count of liked books per author.
    pub favorite_authors: Vec<(String, u32)>,
    /// Most represented decades among liked books (decade start year, count).
    pub preferred_decades: Vec<(i32, u32)>,
    /// Number of books the profile was computed from. Below the display
    /// threshold (5) the dashboard section stays hidden: not enough signal.
    pub scored_books_count: u32,
}

/// Why a book was recommended. Each variant maps to one translated line in
/// the UI: explainability is the trust contract of this feature, a
/// recommendation without a reason must not exist.
#[derive(Debug, Clone, PartialEq)]
pub enum RecommendationReason {
    /// Shares an author with the reference book / taste profile.
    SameAuthor(String),
    /// Shares a shelf or genre label (stored in `books.subjects`).
    SharedSubject(String),
    /// Same publisher as the reference book.
    SamePublisher(String),
    /// Published within ten years of the reference book (ref year, candidate year).
    ClosePeriod(i32, i32),
    /// The candidate itself is liked (rated >= 7, or read, or shelved as favorite).
    HighlyRated,
    /// Already in the reading pile (`to_read` or `wanting`).
    InReadingPile,
}

impl RecommendationReason {
    /// Stable wire identifier, shared by the HTTP JSON shape and the FFI DTO.
    pub fn type_key(&self) -> &'static str {
        match self {
            Self::SameAuthor(_) => "same_author",
            Self::SharedSubject(_) => "shared_subject",
            Self::SamePublisher(_) => "same_publisher",
            Self::ClosePeriod(_, _) => "close_period",
            Self::HighlyRated => "highly_rated",
            Self::InReadingPile => "in_reading_pile",
        }
    }

    /// Display payload for the reason ("Albert Camus", "1942 / 1947", ...).
    /// Empty for reasons that carry no value.
    pub fn value(&self) -> String {
        match self {
            Self::SameAuthor(v) | Self::SharedSubject(v) | Self::SamePublisher(v) => v.clone(),
            Self::ClosePeriod(a, b) => format!("{a} / {b}"),
            Self::HighlyRated | Self::InReadingPile => String::new(),
        }
    }
}

/// One recommendation: a book, its score, and the human-readable reasons
/// that produced the score (strongest first).
#[derive(Debug, Clone)]
pub struct ScoredRecommendation {
    pub book: Book,
    pub score: f64,
    pub reasons: Vec<RecommendationReason>,
}
