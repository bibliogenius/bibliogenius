//! Pure types for the local reading recommendations engine (ADR-059) and
//! the inputs of the external discovery lookups (ADR-060).
//!
//! The scoring computation stays local: no cloud, no profiling, the taste
//! profile is built on-device and never transits. External discovery
//! (series/author completion) sends an anonymous search to the hub: a short
//! ISBN list and a name, never the profile itself (ADR-060 privacy
//! contract). Scoring lives in `services/recommendation_service.rs`; these
//! types are the contract it shares with the API and FFI layers.

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

/// One "complete the series" lookup the client may send to the hub
/// resolver (ADR-060): the anchors identify the series, the member
/// identity lets the client match returned volumes against what is
/// already owned (source ordinals are truth; local `volume_number` is
/// never consulted because frieze reordering renumbers 1..N).
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoverySeriesLookup {
    /// Local `collections.id` of the series-typed collection. Client-side
    /// throttle/cache key; never sent to the hub.
    pub collection_id: String,
    /// User-authored series name, sent as an opaque tiebreaker only.
    pub name: String,
    /// Up to 3 checksum-valid member ISBNs (canonical ISBN-13), the
    /// request anchors.
    pub anchor_isbns: Vec<String>,
    /// All member ISBNs in both ISBN-10/13 forms, for matching returned
    /// volumes against owned members.
    pub member_isbns: Vec<String>,
    /// Normalized "title|author" keys of the members (one per author),
    /// the ISBN-less half of the matching rule.
    pub member_title_author_keys: Vec<String>,
}

/// One "complete the author" lookup (ADR-060, second lane): the display
/// name plus up to 3 anchor ISBNs of liked books by that author. The hub
/// verifies the name against the entity the anchors resolve to.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryAuthorLookup {
    /// Display-cased author name from the taste profile.
    pub name: String,
    /// Up to 3 checksum-valid ISBNs (canonical ISBN-13) of liked books by
    /// this author.
    pub anchor_isbns: Vec<String>,
}

/// Everything the Flutter side needs to run external discovery: the
/// lookups derived from the library, and the library-wide identity index
/// used to filter answers (a returned volume or work matching the index
/// by ISBN or by title+author is never suggested). Empty below the
/// ADR-059 profile threshold: no external lookups without local signal.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryLookupInputs {
    pub series: Vec<DiscoverySeriesLookup>,
    pub authors: Vec<DiscoveryAuthorLookup>,
    /// Every library ISBN (all statuses including `wanting`), expanded to
    /// both ISBN-10/13 forms, sorted and deduplicated.
    pub library_isbns: Vec<String>,
    /// Normalized "title|author" keys for every library book (one per
    /// author), sorted and deduplicated.
    pub library_title_author_keys: Vec<String>,
}
