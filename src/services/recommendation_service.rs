//! Local reading recommendations engine (ADR-059).
//!
//! Scores books from the user's own library against a reference book
//! ("You might also like", book details) or against a taste profile
//! ("Suggestions for you", dashboard). The scoring is computed in-memory
//! and on-device from data the library already holds: no cloud, no
//! profiling. External discovery (ADR-060) sends an anonymous search to
//! the hub from the client side; the profile built here never transits.
//! Explainability is the trust contract: every recommendation carries the
//! human-readable reasons that produced it.
//!
//! Data-coverage reality this engine is tuned for (field audit, 2026-08):
//! ratings and Dewey codes are almost never filled, `book_tags` is unused
//! (shelves live in `books.subjects`, including the closed genre list),
//! authors are the densest signal, year/publisher sit around 60%. Hence:
//! - "liked" falls back to `reading_status = 'read'` and to a favorites
//!   shelf when no rating exists;
//! - genres are matched through `subjects` like any other shelf label, at
//!   the same weight (they ARE subjects rows, ADR-052 adjacent design);
//! - there is no `book_tags` overlap signal and no language signal
//!   (`books.language` does not exist as a column).
//!
//! Reads RAW `books.reading_status` values (the 5 stored ones): the
//! repository read path overlays `borrowed`/`lent` for display, which
//! would hide the real value and corrupt both the profile and the
//! candidate filter, so this service loads entities directly.

use std::collections::{HashMap, HashSet};

use sea_orm::{DatabaseConnection, EntityTrait};

use crate::domain::recommendations::{RecommendationReason, ScoredRecommendation, TasteProfile};
use crate::models::Book;
use crate::services::book_service::ServiceError;

/// Default result sizes, per the product spec.
pub const SIMILAR_DEFAULT_LIMIT: usize = 5;
pub const PERSONAL_DEFAULT_LIMIT: usize = 10;

/// The dashboard section stays hidden below this profile size: too little
/// signal to score meaningfully.
pub const MIN_SCORED_BOOKS: u32 = 5;

/// Scoring weights. Not user-configurable by decision (ADR-059): tuning
/// knobs nobody understands are over-engineering for a home library.
const W_SAME_AUTHOR: f64 = 3.0;
const W_SHARED_SUBJECT: f64 = 1.0;
const SUBJECT_SCORE_CAP: f64 = 5.0;
const W_SAME_PUBLISHER: f64 = 0.5;
const W_CLOSE_PERIOD: f64 = 0.5;
const W_LIKED_CANDIDATE: f64 = 1.0;
/// Deprioritizes already-read candidates in "similar to this book": the
/// reminder of a liked same-universe book keeps value for a reader who does
/// not remember everything, but it must not squat the few slots ahead of
/// unread books. Sized to outweigh the liked fallback bonus a read book
/// otherwise gets (net -0.5), without ever excluding it (ADR-059).
const READ_CANDIDATE_PENALTY: f64 = -1.5;
const W_DEWEY_MAJOR: f64 = 1.0;
const W_FAVORITE_AUTHOR: f64 = 2.0;
const W_PREFERRED_DECADE: f64 = 0.5;
const W_IN_PILE: f64 = 1.0;
const W_RATED_UNREAD: f64 = 0.5;
/// Rank-decayed weight of a profile subject match: 1.0, 0.8, 0.6, ... with
/// a floor so long-tail subjects still count a little.
const SUBJECT_RANK_STEP: f64 = 0.2;
const SUBJECT_RANK_FLOOR: f64 = 0.2;

/// Years within which two publication dates count as "same period".
const CLOSE_PERIOD_YEARS: i32 = 10;

/// A rating at or above this (0-10 scale) means "liked".
const LIKED_RATING_MIN: i32 = 7;

/// Shelf labels that mark a book as a favorite, compared normalized.
/// Favorites are ordinary shelves (there is no first-class flag), so the
/// engine recognizes the common labels a user would type.
const FAVORITE_SHELF_LABELS: [&str; 5] =
    ["favoris", "favori", "favorites", "favorite", "favourites"];

/// How many reasons of one kind a single recommendation may carry; keeps
/// the UI line short when two books share many shelves.
const MAX_SUBJECT_REASONS: usize = 3;

/// One book prepared for scoring: the output DTO plus the normalized,
/// raw-status view of the signals. Built once per request for the whole
/// library, then scored in-memory (V1 design: no repository extension,
/// instant for the hundreds-of-books libraries the app targets).
#[derive(Debug, Clone)]
pub struct ScoringBook {
    pub book: Book,
    /// RAW `books.reading_status` (the 5 stored values), never the
    /// borrowed/lent display overlay.
    pub raw_status: String,
    /// Trimmed, lowercased subject labels (shelves + genres).
    pub norm_subjects: Vec<String>,
    /// Trimmed, lowercased author names.
    pub norm_authors: Vec<String>,
    pub norm_publisher: Option<String>,
    pub decade: Option<i32>,
    pub dewey_major: Option<char>,
    /// One of the shelves is a favorites label. Kept OUT of
    /// [`Self::norm_subjects`]: it feeds the liked signal only.
    pub has_favorite_shelf: bool,
}

pub(crate) fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

fn non_empty_norm(s: Option<&str>) -> Option<String> {
    s.map(norm).filter(|v| !v.is_empty())
}

/// First digit of the Dewey code = its major class ("8" for literature).
fn dewey_major_class(dewey: Option<&str>) -> Option<char> {
    dewey
        .map(str::trim)
        .and_then(|d| d.chars().next())
        .filter(char::is_ascii_digit)
}

fn decade_of(year: Option<i32>) -> Option<i32> {
    year.map(|y| y - y.rem_euclid(10))
}

/// A favorites shelf marks affection, not theme: it must feed the liked
/// signal and NOTHING else. Left among the scoring subjects it would make
/// any two favorites count as thematically similar ("shared subject:
/// favoris"), which is exactly the poisoning the favorites-as-collection
/// design forbids.
fn is_favorite_label(normed: &str) -> bool {
    FAVORITE_SHELF_LABELS.contains(&normed)
}

impl ScoringBook {
    pub fn new(book: Book, raw_status: String) -> Self {
        let all_subjects: Vec<String> = book
            .subjects
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| norm(s))
            .filter(|s| !s.is_empty())
            .collect();
        let has_favorite_shelf = all_subjects.iter().any(|s| is_favorite_label(s));
        let norm_subjects = all_subjects
            .into_iter()
            .filter(|s| !is_favorite_label(s))
            .collect();
        let norm_authors = book
            .authors
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|a| norm(a))
            .filter(|a| !a.is_empty())
            .collect();
        let norm_publisher = non_empty_norm(book.publisher.as_deref());
        let decade = decade_of(book.publication_year);
        let dewey_major = dewey_major_class(book.dewey_decimal.as_deref());
        Self {
            book,
            raw_status,
            norm_subjects,
            norm_authors,
            norm_publisher,
            decade,
            dewey_major,
            has_favorite_shelf,
        }
    }

    /// Whether the user liked this book. With ratings almost never filled,
    /// "liked" falls back to having read the book, or having shelved it as
    /// a favorite (ADR-059). An explicit low rating vetoes the fallbacks.
    ///
    /// When the typed favorites collection ships (`collections.source =
    /// 'favorites'`, series-pattern), membership in it must join this test;
    /// the shelf-label fallback then remains for legacy shelves only.
    pub fn is_liked(&self) -> bool {
        match self.book.user_rating {
            Some(r) => r >= LIKED_RATING_MIN,
            None => self.raw_status == "read" || self.has_favorite_shelf,
        }
    }

    /// Books that feed the taste profile: read or being read, explicitly
    /// rated, or shelved as favorites.
    fn feeds_profile(&self) -> bool {
        self.raw_status == "read"
            || self.raw_status == "reading"
            || self.book.user_rating.is_some()
            || self.is_liked()
    }

    /// Candidates for the dashboard "For you" list: unread books only
    /// (decision, ADR-059). `read` and `abandoned` are out by decision,
    /// `reading` because it is already in the user's hands.
    fn is_personal_candidate(&self) -> bool {
        self.raw_status == "to_read" || self.raw_status == "wanting"
    }
}

/// Load the whole library as scoring rows: raw entity rows (bypassing the
/// borrowed/lent display overlay) plus a batch author join.
pub async fn load_scoring_books(db: &DatabaseConnection) -> Result<Vec<ScoringBook>, ServiceError> {
    use crate::models::{author, book, book_authors};

    let models = book::Entity::find().all(db).await?;
    let links = book_authors::Entity::find().all(db).await?;
    let authors = author::Entity::find().all(db).await?;

    let name_by_id: HashMap<String, String> = authors.into_iter().map(|a| (a.id, a.name)).collect();
    let mut authors_by_book: HashMap<String, Vec<String>> = HashMap::new();
    for link in links {
        if let Some(name) = name_by_id.get(&link.author_id) {
            authors_by_book
                .entry(link.book_id)
                .or_default()
                .push(name.clone());
        }
    }

    Ok(models
        .into_iter()
        .map(|model| {
            let raw_status = model.reading_status.clone();
            let id = model.id.clone();
            // Book::from keeps the raw status; the overlay only happens in
            // populate_authors, which is deliberately NOT used here.
            let mut book = Book::from(model);
            if let Some(names) = authors_by_book.remove(&id) {
                book.author = Some(names.join(", "));
                book.authors = Some(names);
            }
            ScoringBook::new(book, raw_status)
        })
        .collect())
}

/// Build the taste profile from the library (pure part).
pub fn build_taste_profile(books: &[ScoringBook]) -> TasteProfile {
    let profile_books: Vec<&ScoringBook> = books.iter().filter(|b| b.feeds_profile()).collect();

    // Subjects aggregate over the whole profile set; keep the original
    // casing of the first occurrence for display.
    let mut subject_counts: HashMap<String, (String, u32)> = HashMap::new();
    let mut author_counts: HashMap<String, (String, u32)> = HashMap::new();
    let mut decade_counts: HashMap<i32, u32> = HashMap::new();

    for sb in &profile_books {
        for (raw, normed) in sb
            .book
            .subjects
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| (s.trim(), norm(s)))
            .filter(|(_, n)| !n.is_empty() && !is_favorite_label(n))
        {
            let entry = subject_counts
                .entry(normed)
                .or_insert_with(|| (raw.to_string(), 0));
            entry.1 += 1;
        }
        if let Some(decade) = sb.decade {
            *decade_counts.entry(decade).or_insert(0) += 1;
        }
        // Favorite authors: authors of LIKED books. With ratings unused,
        // the spec's "avg rating >= 7" arm never fires, so the liked
        // fallback (read / favorites shelf) carries this signal.
        if sb.is_liked() {
            for (raw, normed) in sb
                .book
                .authors
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|a| (a.trim(), norm(a)))
                .filter(|(_, n)| !n.is_empty())
            {
                let entry = author_counts
                    .entry(normed)
                    .or_insert_with(|| (raw.to_string(), 0));
                entry.1 += 1;
            }
        }
    }

    let mut top_subjects: Vec<(String, u32)> = subject_counts.into_values().collect();
    top_subjects.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut favorite_authors: Vec<(String, u32)> = author_counts.into_values().collect();
    favorite_authors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut preferred_decades: Vec<(i32, u32)> = decade_counts.into_iter().collect();
    preferred_decades.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    preferred_decades.truncate(3);

    TasteProfile {
        top_subjects,
        favorite_authors,
        preferred_decades,
        scored_books_count: profile_books.len() as u32,
    }
}

/// Score one candidate against a reference book. Returns `None` when the
/// pair shares no content signal (no author overlap AND no subject
/// overlap): publisher or period alone must not surface a book, and a
/// candidate with neither authors nor subjects is unexplainable.
fn score_against_reference(
    reference: &ScoringBook,
    candidate: &ScoringBook,
) -> Option<(f64, Vec<RecommendationReason>)> {
    let mut score = 0.0;
    let mut reasons = Vec::new();

    // Same author: flat bonus, not per shared name.
    let ref_authors: HashSet<&str> = reference.norm_authors.iter().map(String::as_str).collect();
    let shared_author = candidate
        .norm_authors
        .iter()
        .position(|a| ref_authors.contains(a.as_str()));
    if let Some(idx) = shared_author {
        score += W_SAME_AUTHOR;
        let display = candidate
            .book
            .authors
            .as_deref()
            .and_then(|a| a.get(idx))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        reasons.push(RecommendationReason::SameAuthor(display));
    }

    // Shared shelves/genres, normalized matching (trim + case-insensitive,
    // no fuzzy matching in V1 by decision).
    let ref_subjects: HashSet<&str> = reference.norm_subjects.iter().map(String::as_str).collect();
    let mut subject_score = 0.0;
    let mut subject_reasons = 0usize;
    for (idx, normed) in candidate.norm_subjects.iter().enumerate() {
        if ref_subjects.contains(normed.as_str()) {
            subject_score += W_SHARED_SUBJECT;
            if subject_reasons < MAX_SUBJECT_REASONS {
                let display = candidate
                    .book
                    .subjects
                    .as_deref()
                    .and_then(|s| s.get(idx))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                reasons.push(RecommendationReason::SharedSubject(display));
                subject_reasons += 1;
            }
        }
    }
    score += subject_score.min(SUBJECT_SCORE_CAP);

    // Content-signal gate: author or subject overlap is required.
    if shared_author.is_none() && subject_reasons == 0 {
        return None;
    }

    if let (Some(a), Some(b)) = (&reference.norm_publisher, &candidate.norm_publisher)
        && a == b
    {
        score += W_SAME_PUBLISHER;
        let display = candidate
            .book
            .publisher
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        reasons.push(RecommendationReason::SamePublisher(display));
    }

    if let (Some(ya), Some(yb)) = (
        reference.book.publication_year,
        candidate.book.publication_year,
    ) && (ya - yb).abs() <= CLOSE_PERIOD_YEARS
    {
        score += W_CLOSE_PERIOD;
        reasons.push(RecommendationReason::ClosePeriod(ya, yb));
    }

    if candidate.is_liked() {
        score += W_LIKED_CANDIDATE;
        // The reason line says "you rated this highly": only honest when an
        // actual rating exists. The read/favorites fallback still boosts the
        // score but stays silent.
        if candidate
            .book
            .user_rating
            .is_some_and(|r| r >= LIKED_RATING_MIN)
        {
            reasons.push(RecommendationReason::HighlyRated);
        }
    }

    // Already read: deprioritized, never excluded. At equal signal an
    // unread book must win the slot.
    if candidate.raw_status == "read" {
        score += READ_CANDIDATE_PENALTY;
    }

    // Dewey major class: precise when present, neutral when absent (most
    // libraries never fill it).
    if let (Some(a), Some(b)) = (reference.dewey_major, candidate.dewey_major)
        && a == b
    {
        score += W_DEWEY_MAJOR;
    }

    Some((score, reasons))
}

/// Score one candidate against the taste profile. Returns `None` without a
/// content signal (profile subject or favorite author), mirroring
/// [`score_against_reference`].
fn score_against_profile(
    profile: &TasteProfile,
    candidate: &ScoringBook,
) -> Option<(f64, Vec<RecommendationReason>)> {
    let mut score = 0.0;
    let mut reasons = Vec::new();

    // Rank map of the profile's subjects: top subject weighs full, then
    // decays by rank down to a floor.
    let subject_rank: HashMap<String, usize> = profile
        .top_subjects
        .iter()
        .enumerate()
        .map(|(rank, (label, _))| (norm(label), rank))
        .collect();

    let mut matched_subject = false;
    let mut subject_reasons = 0usize;
    for (idx, normed) in candidate.norm_subjects.iter().enumerate() {
        if let Some(rank) = subject_rank.get(normed) {
            matched_subject = true;
            let weight =
                (W_SHARED_SUBJECT - SUBJECT_RANK_STEP * *rank as f64).max(SUBJECT_RANK_FLOOR);
            score += weight;
            if subject_reasons < MAX_SUBJECT_REASONS {
                let display = candidate
                    .book
                    .subjects
                    .as_deref()
                    .and_then(|s| s.get(idx))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                reasons.push(RecommendationReason::SharedSubject(display));
                subject_reasons += 1;
            }
        }
    }

    let favorite_authors: HashSet<String> = profile
        .favorite_authors
        .iter()
        .map(|(name, _)| norm(name))
        .collect();
    let matched_author = candidate
        .norm_authors
        .iter()
        .position(|a| favorite_authors.contains(a));
    if let Some(idx) = matched_author {
        score += W_FAVORITE_AUTHOR;
        let display = candidate
            .book
            .authors
            .as_deref()
            .and_then(|a| a.get(idx))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        reasons.push(RecommendationReason::SameAuthor(display));
    }

    if !matched_subject && matched_author.is_none() {
        return None;
    }

    if let Some(decade) = candidate.decade
        && profile.preferred_decades.iter().any(|(d, _)| *d == decade)
    {
        score += W_PREFERRED_DECADE;
    }

    // Owned pile beats the wishlist: for "read what is already at home",
    // a to_read book on the shelf outranks a wished one (ADR-059).
    if candidate.raw_status == "to_read" {
        score += W_IN_PILE;
        reasons.push(RecommendationReason::InReadingPile);
    }

    // Rated but not read (borrowed once? re-shelved?): mild interest signal.
    if candidate.book.user_rating.is_some() {
        score += W_RATED_UNREAD;
    }

    Some((score, reasons))
}

/// Shared tie-break: score desc, then rating desc, then most recent year,
/// then title (spec section 9).
fn sort_and_truncate(
    mut recs: Vec<ScoredRecommendation>,
    limit: usize,
) -> Vec<ScoredRecommendation> {
    recs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.book.user_rating.cmp(&a.book.user_rating))
            .then_with(|| b.book.publication_year.cmp(&a.book.publication_year))
            .then_with(|| a.book.title.cmp(&b.book.title))
    });
    recs.truncate(limit);
    recs
}

/// "You might also like": books similar to a reference book (pure part).
pub fn similar_books(
    books: &[ScoringBook],
    reference_id: &str,
    limit: usize,
) -> Vec<ScoredRecommendation> {
    let Some(reference) = books
        .iter()
        .find(|b| b.book.id.as_deref() == Some(reference_id))
    else {
        return Vec::new();
    };

    let recs = books
        .iter()
        .filter(|c| c.book.id.as_deref() != Some(reference_id))
        .filter_map(|c| {
            score_against_reference(reference, c).map(|(score, reasons)| ScoredRecommendation {
                book: c.book.clone(),
                score,
                reasons,
            })
        })
        .collect();
    sort_and_truncate(recs, limit)
}

/// "Suggestions for you": unread books scored against the taste profile
/// (pure part). Returns the profile alongside so callers can surface it.
pub fn personal_suggestions(
    books: &[ScoringBook],
    limit: usize,
) -> (TasteProfile, Vec<ScoredRecommendation>) {
    let profile = build_taste_profile(books);
    if profile.scored_books_count < MIN_SCORED_BOOKS {
        return (profile, Vec::new());
    }

    let recs = books
        .iter()
        .filter(|c| c.is_personal_candidate())
        .filter_map(|c| {
            score_against_profile(&profile, c).map(|(score, reasons)| ScoredRecommendation {
                book: c.book.clone(),
                score,
                reasons,
            })
        })
        .collect();
    (profile, sort_and_truncate(recs, limit))
}

/// DB-facing wrapper for the book-details section.
pub async fn similar_to(
    db: &DatabaseConnection,
    book_id: &str,
    limit: Option<usize>,
) -> Result<Vec<ScoredRecommendation>, ServiceError> {
    let books = load_scoring_books(db).await?;
    Ok(similar_books(
        &books,
        book_id,
        limit.unwrap_or(SIMILAR_DEFAULT_LIMIT),
    ))
}

/// DB-facing wrapper for the dashboard section.
pub async fn suggestions_for_user(
    db: &DatabaseConnection,
    limit: Option<usize>,
) -> Result<(TasteProfile, Vec<ScoredRecommendation>), ServiceError> {
    let books = load_scoring_books(db).await?;
    Ok(personal_suggestions(
        &books,
        limit.unwrap_or(PERSONAL_DEFAULT_LIMIT),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(id: &str, title: &str) -> Book {
        Book {
            id: Some(id.to_string()),
            title: title.to_string(),
            ..Default::default()
        }
    }

    struct Builder {
        book: Book,
        status: String,
    }

    fn sb(id: &str, title: &str) -> Builder {
        Builder {
            book: book(id, title),
            status: "to_read".to_string(),
        }
    }

    impl Builder {
        fn authors(mut self, authors: &[&str]) -> Self {
            self.book.authors = Some(authors.iter().map(|s| s.to_string()).collect());
            self
        }
        fn subjects(mut self, subjects: &[&str]) -> Self {
            self.book.subjects = Some(subjects.iter().map(|s| s.to_string()).collect());
            self
        }
        fn publisher(mut self, p: &str) -> Self {
            self.book.publisher = Some(p.to_string());
            self
        }
        fn year(mut self, y: i32) -> Self {
            self.book.publication_year = Some(y);
            self
        }
        fn dewey(mut self, d: &str) -> Self {
            self.book.dewey_decimal = Some(d.to_string());
            self
        }
        fn rating(mut self, r: i32) -> Self {
            self.book.user_rating = Some(r);
            self
        }
        fn status(mut self, s: &str) -> Self {
            self.status = s.to_string();
            self
        }
        fn build(self) -> ScoringBook {
            ScoringBook::new(self.book, self.status)
        }
    }

    fn score_pair(reference: &ScoringBook, candidate: &ScoringBook) -> Option<f64> {
        score_against_reference(reference, candidate).map(|(s, _)| s)
    }

    // ── similar-to component weights ────────────────────────────────

    #[test]
    fn same_author_scores_flat_bonus_with_reason() {
        let r = sb("r", "Ref").authors(&["Albert Camus"]).build();
        let c = sb("c", "Cand").authors(&["Albert Camus"]).build();
        let (score, reasons) = score_against_reference(&r, &c).unwrap();
        assert_eq!(score, 3.0);
        assert_eq!(
            reasons,
            vec![RecommendationReason::SameAuthor("Albert Camus".into())]
        );
    }

    #[test]
    fn author_match_is_trimmed_and_case_insensitive() {
        let r = sb("r", "Ref").authors(&["  albert CAMUS "]).build();
        let c = sb("c", "Cand").authors(&["Albert Camus"]).build();
        assert_eq!(score_pair(&r, &c), Some(3.0));
    }

    #[test]
    fn subject_overlap_scores_per_match_and_caps() {
        let subjects = ["a", "b", "c", "d", "e", "f", "g"];
        let r = sb("r", "Ref").subjects(&subjects).build();
        let c = sb("c", "Cand").subjects(&subjects).build();
        // 7 matches at +1.0 but capped at +5.0.
        assert_eq!(score_pair(&r, &c), Some(5.0));
    }

    #[test]
    fn subject_match_normalizes_and_keeps_display_casing() {
        let r = sb("r", "Ref").subjects(&["Science Fiction"]).build();
        let c = sb("c", "Cand").subjects(&["  science fiction "]).build();
        let (score, reasons) = score_against_reference(&r, &c).unwrap();
        assert_eq!(score, 1.0);
        assert_eq!(
            reasons,
            vec![RecommendationReason::SharedSubject(
                "science fiction".into()
            )]
        );
    }

    #[test]
    fn genre_labels_in_subjects_count_like_any_subject() {
        // Genres are stored as shelf labels inside `subjects`: same signal,
        // same weight.
        let r = sb("r", "Ref").subjects(&["Roman"]).build();
        let c = sb("c", "Cand").subjects(&["roman"]).build();
        assert_eq!(score_pair(&r, &c), Some(1.0));
    }

    #[test]
    fn publisher_and_period_are_boosters_not_gates() {
        // Publisher + close period with NO author/subject overlap: excluded.
        let r = sb("r", "Ref")
            .authors(&["A"])
            .publisher("Gallimard")
            .year(1950)
            .build();
        let c = sb("c", "Cand")
            .authors(&["B"])
            .publisher("Gallimard")
            .year(1955)
            .build();
        assert!(score_against_reference(&r, &c).is_none());

        // With an author match they add +0.5 each.
        let c2 = sb("c2", "Cand2")
            .authors(&["A"])
            .publisher("Gallimard")
            .year(1955)
            .build();
        assert_eq!(score_pair(&r, &c2), Some(3.0 + 0.5 + 0.5));
    }

    #[test]
    fn close_period_boundary_is_ten_years() {
        let r = sb("r", "Ref").subjects(&["x"]).year(1950).build();
        let at = sb("a", "A").subjects(&["x"]).year(1960).build();
        let beyond = sb("b", "B").subjects(&["x"]).year(1961).build();
        assert_eq!(score_pair(&r, &at), Some(1.5));
        assert_eq!(score_pair(&r, &beyond), Some(1.0));
    }

    #[test]
    fn liked_candidate_via_rating_adds_bonus_and_reason() {
        let r = sb("r", "Ref").subjects(&["x"]).build();
        let c = sb("c", "Cand").subjects(&["x"]).rating(9).build();
        let (score, reasons) = score_against_reference(&r, &c).unwrap();
        assert_eq!(score, 2.0);
        assert!(reasons.contains(&RecommendationReason::HighlyRated));
    }

    #[test]
    fn liked_candidate_via_read_fallback_boosts_without_reason() {
        // No rating anywhere in the library: "liked" falls back to read.
        // Net for a read candidate: +1.0 subject +1.0 liked -1.5 read.
        let r = sb("r", "Ref").subjects(&["x"]).build();
        let c = sb("c", "Cand").subjects(&["x"]).status("read").build();
        let (score, reasons) = score_against_reference(&r, &c).unwrap();
        assert_eq!(score, 0.5);
        assert!(!reasons.contains(&RecommendationReason::HighlyRated));
    }

    #[test]
    fn read_candidate_is_deprioritized_but_never_excluded() {
        let books = vec![
            sb("r", "Ref").authors(&["A"]).build(),
            sb("read", "Read").authors(&["A"]).status("read").build(),
            sb("unread", "Unread").authors(&["A"]).build(),
        ];
        let recs = similar_books(&books, "r", 5);
        // At equal signal the unread book wins the slot; the read one still
        // surfaces behind it (revisiting keeps value, it just cannot squat).
        let ids: Vec<&str> = recs.iter().filter_map(|b| b.book.id.as_deref()).collect();
        assert_eq!(ids, vec!["unread", "read"]);
    }

    #[test]
    fn liked_candidate_via_favorites_shelf() {
        let c = sb("c", "Cand").subjects(&["Favoris"]).build();
        assert!(c.is_liked());
        // An explicit low rating vetoes the fallback.
        let vetoed = sb("v", "V").subjects(&["Favoris"]).rating(3).build();
        assert!(!vetoed.is_liked());
    }

    #[test]
    fn favorites_shelf_never_counts_as_thematic_overlap() {
        // Two books sharing ONLY a favorites shelf are not thematically
        // similar: the label feeds the liked signal, nothing else. Without
        // this, any two favorites would recommend each other with a bogus
        // "shared subject: favoris" reason.
        let r = sb("r", "Ref").subjects(&["Favoris"]).build();
        let c = sb("c", "Cand").subjects(&["favoris"]).build();
        assert!(score_against_reference(&r, &c).is_none());
        assert!(r.is_liked() && c.is_liked());

        // And the taste profile never elects it as a top subject.
        let books = vec![
            sb("1", "A")
                .status("read")
                .subjects(&["favoris", "SF"])
                .build(),
            sb("2", "B").status("read").subjects(&["favoris"]).build(),
        ];
        let profile = build_taste_profile(&books);
        assert_eq!(profile.top_subjects, vec![("SF".to_string(), 1)]);
    }

    #[test]
    fn dewey_major_class_matches_or_stays_neutral() {
        let r = sb("r", "Ref").subjects(&["x"]).dewey("843.91").build();
        let same = sb("a", "A").subjects(&["x"]).dewey("848").build();
        let other = sb("b", "B").subjects(&["x"]).dewey("510").build();
        let absent = sb("c", "C").subjects(&["x"]).build();
        assert_eq!(score_pair(&r, &same), Some(2.0));
        assert_eq!(score_pair(&r, &other), Some(1.0));
        assert_eq!(score_pair(&r, &absent), Some(1.0));
    }

    // ── similar-to edge cases (spec section 9) ──────────────────────

    #[test]
    fn candidate_without_author_or_subject_overlap_is_excluded() {
        let r = sb("r", "Ref").authors(&["A"]).subjects(&["x"]).build();
        let empty = sb("c", "Cand").build();
        assert!(score_against_reference(&r, &empty).is_none());
    }

    #[test]
    fn no_subjects_falls_back_to_author_only() {
        let r = sb("r", "Ref").authors(&["A"]).build();
        let c = sb("c", "Cand").authors(&["A"]).build();
        assert_eq!(score_pair(&r, &c), Some(3.0));
    }

    #[test]
    fn reference_book_is_excluded_from_results() {
        let books = vec![
            sb("1", "One").authors(&["A"]).build(),
            sb("2", "Two").authors(&["A"]).build(),
        ];
        let recs = similar_books(&books, "1", 5);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].book.id.as_deref(), Some("2"));
    }

    #[test]
    fn unknown_reference_returns_empty() {
        let books = vec![sb("1", "One").authors(&["A"]).build()];
        assert!(similar_books(&books, "nope", 5).is_empty());
    }

    #[test]
    fn all_same_author_library_ranks_by_other_signals() {
        // Flat author score: subjects must break the tie.
        let books = vec![
            sb("r", "Ref").authors(&["A"]).subjects(&["sf"]).build(),
            sb("1", "Plain").authors(&["A"]).build(),
            sb("2", "Match").authors(&["A"]).subjects(&["sf"]).build(),
        ];
        let recs = similar_books(&books, "r", 5);
        assert_eq!(recs[0].book.id.as_deref(), Some("2"));
        assert_eq!(recs[1].book.id.as_deref(), Some("1"));
    }

    #[test]
    fn ties_break_by_rating_then_recent_year_then_title() {
        let books = vec![
            sb("r", "Ref").subjects(&["x"]).build(),
            sb("1", "Bbb").subjects(&["x"]).year(1990).build(),
            sb("2", "Aaa").subjects(&["x"]).year(1990).build(),
            sb("3", "Ccc").subjects(&["x"]).year(2020).build(),
            //
            sb("4", "Ddd").subjects(&["x"]).year(1980).rating(8).build(),
        ];
        let recs = similar_books(&books, "r", 5);
        let ids: Vec<&str> = recs.iter().filter_map(|r| r.book.id.as_deref()).collect();
        // Rated book first (rating bonus + tie-break), then recent year,
        // then alphabetical title.
        assert_eq!(ids, vec!["4", "3", "2", "1"]);
    }

    #[test]
    fn limit_is_applied() {
        let books = vec![
            sb("r", "Ref").subjects(&["x"]).build(),
            sb("1", "A").subjects(&["x"]).build(),
            sb("2", "B").subjects(&["x"]).build(),
            sb("3", "C").subjects(&["x"]).build(),
        ];
        assert_eq!(similar_books(&books, "r", 2).len(), 2);
    }

    // ── taste profile ───────────────────────────────────────────────

    #[test]
    fn profile_counts_read_reading_rated_and_favorites() {
        let books = vec![
            sb("1", "Read").status("read").build(),
            sb("2", "Reading").status("reading").build(),
            sb("3", "Rated").rating(5).build(),
            sb("4", "Fav").subjects(&["favoris"]).build(),
            sb("5", "Pile").build(),
            sb("6", "Wish").status("wanting").build(),
        ];
        let profile = build_taste_profile(&books);
        assert_eq!(profile.scored_books_count, 4);
    }

    #[test]
    fn favorite_authors_come_from_liked_books_without_any_rating() {
        // The stated use case: books read at home, never rated.
        let books = vec![
            sb("1", "One").status("read").authors(&["Camus"]).build(),
            sb("2", "Two").status("read").authors(&["Camus"]).build(),
            sb("3", "Three")
                .status("reading")
                .authors(&["Herbert"])
                .build(),
        ];
        let profile = build_taste_profile(&books);
        // Reading feeds the profile subjects but is not "liked": only read
        // books elect favorite authors here.
        assert_eq!(profile.favorite_authors, vec![("Camus".to_string(), 2)]);
    }

    #[test]
    fn profile_orders_subjects_by_frequency() {
        let books = vec![
            sb("1", "A")
                .status("read")
                .subjects(&["SF", "Roman"])
                .build(),
            sb("2", "B").status("read").subjects(&["SF"]).build(),
        ];
        let profile = build_taste_profile(&books);
        assert_eq!(profile.top_subjects[0], ("SF".to_string(), 2));
        assert_eq!(profile.top_subjects[1], ("Roman".to_string(), 1));
    }

    #[test]
    fn profile_decades_are_grouped_and_capped() {
        let books = vec![
            sb("1", "A").status("read").year(1961).build(),
            sb("2", "B").status("read").year(1969).build(),
            sb("3", "C").status("read").year(2011).build(),
            sb("4", "D").status("read").year(1994).build(),
            sb("5", "E").status("read").year(1983).build(),
        ];
        let profile = build_taste_profile(&books);
        assert_eq!(profile.preferred_decades.len(), 3);
        assert_eq!(profile.preferred_decades[0], (1960, 2));
    }

    // ── personal suggestions ────────────────────────────────────────

    /// A liked base of 5 read Camus books shelved "philosophie", so the
    /// profile passes MIN_SCORED_BOOKS.
    fn liked_base() -> Vec<ScoringBook> {
        (0..5)
            .map(|i| {
                sb(&format!("base{i}"), &format!("Base {i}"))
                    .status("read")
                    .authors(&["Camus"])
                    .subjects(&["philosophie"])
                    .build()
            })
            .collect()
    }

    #[test]
    fn personal_hides_below_min_scored_books() {
        let books = vec![
            sb("1", "Read").status("read").subjects(&["x"]).build(),
            sb("2", "Pile").subjects(&["x"]).build(),
        ];
        let (profile, recs) = personal_suggestions(&books, 10);
        assert_eq!(profile.scored_books_count, 1);
        assert!(recs.is_empty());
    }

    #[test]
    fn personal_only_suggests_unread_candidates() {
        let mut books = liked_base();
        books.push(
            sb("read", "Read")
                .status("read")
                .subjects(&["philosophie"])
                .build(),
        );
        books.push(
            sb("abandoned", "Abandoned")
                .status("abandoned")
                .subjects(&["philosophie"])
                .build(),
        );
        books.push(
            sb("reading", "Reading")
                .status("reading")
                .subjects(&["philosophie"])
                .build(),
        );
        books.push(sb("pile", "Pile").subjects(&["philosophie"]).build());
        books.push(
            sb("wish", "Wish")
                .status("wanting")
                .subjects(&["philosophie"])
                .build(),
        );
        let (_, recs) = personal_suggestions(&books, 10);
        let ids: Vec<&str> = recs.iter().filter_map(|r| r.book.id.as_deref()).collect();
        assert_eq!(ids, vec!["pile", "wish"]);
    }

    #[test]
    fn personal_favorite_author_scores_and_explains() {
        let mut books = liked_base();
        books.push(sb("c", "Cand").authors(&["Camus"]).build());
        let (_, recs) = personal_suggestions(&books, 10);
        assert_eq!(recs.len(), 1);
        // +2.0 favorite author, +1.0 in pile (to_read).
        assert_eq!(recs[0].score, 3.0);
        assert!(
            recs[0]
                .reasons
                .contains(&RecommendationReason::SameAuthor("Camus".into()))
        );
        assert!(
            recs[0]
                .reasons
                .contains(&RecommendationReason::InReadingPile)
        );
    }

    #[test]
    fn personal_subject_weight_decays_by_rank() {
        let mut books: Vec<ScoringBook> = Vec::new();
        // "alpha" x3, "beta" x2, "gamma" x1 in the liked base.
        for i in 0..3 {
            books.push(
                sb(&format!("a{i}"), "A")
                    .status("read")
                    .subjects(&["alpha"])
                    .build(),
            );
        }
        for i in 0..2 {
            books.push(
                sb(&format!("b{i}"), "B")
                    .status("read")
                    .subjects(&["beta"])
                    .build(),
            );
        }
        books.push(sb("g", "G").status("read").subjects(&["gamma"]).build());

        books.push(
            sb("c1", "C1")
                .status("wanting")
                .subjects(&["alpha"])
                .build(),
        );
        books.push(sb("c2", "C2").status("wanting").subjects(&["beta"]).build());
        books.push(
            sb("c3", "C3")
                .status("wanting")
                .subjects(&["gamma"])
                .build(),
        );
        let (_, recs) = personal_suggestions(&books, 10);
        let score_of = |id: &str| {
            recs.iter()
                .find(|r| r.book.id.as_deref() == Some(id))
                .unwrap()
                .score
        };
        assert_eq!(score_of("c1"), 1.0);
        assert_eq!(score_of("c2"), 0.8);
        assert_eq!(score_of("c3"), 0.6);
    }

    #[test]
    fn personal_pile_beats_wishlist_on_equal_signal() {
        let mut books = liked_base();
        books.push(sb("pile", "Pile").subjects(&["philosophie"]).build());
        books.push(
            sb("wish", "Wish")
                .status("wanting")
                .subjects(&["philosophie"])
                .build(),
        );
        let (_, recs) = personal_suggestions(&books, 10);
        assert_eq!(recs[0].book.id.as_deref(), Some("pile"));
        assert!(recs[0].score > recs[1].score);
    }

    #[test]
    fn personal_candidate_without_signal_is_excluded() {
        let mut books = liked_base();
        books.push(sb("blank", "Blank").year(1960).build());
        let (_, recs) = personal_suggestions(&books, 10);
        assert!(recs.iter().all(|r| r.book.id.as_deref() != Some("blank")));
    }

    #[test]
    fn personal_preferred_decade_boosts() {
        let mut books: Vec<ScoringBook> = (0..5)
            .map(|i| {
                sb(&format!("b{i}"), "B")
                    .status("read")
                    .subjects(&["sf"])
                    .year(1965)
                    .build()
            })
            .collect();
        books.push(sb("in", "In").subjects(&["sf"]).year(1968).build());
        books.push(sb("out", "Out").subjects(&["sf"]).year(1990).build());
        let (_, recs) = personal_suggestions(&books, 10);
        let score_of = |id: &str| {
            recs.iter()
                .find(|r| r.book.id.as_deref() == Some(id))
                .unwrap()
                .score
        };
        assert_eq!(score_of("in") - score_of("out"), 0.5);
    }

    // ── reason wire keys ────────────────────────────────────────────

    #[test]
    fn reason_type_keys_are_stable() {
        assert_eq!(
            RecommendationReason::SameAuthor("x".into()).type_key(),
            "same_author"
        );
        assert_eq!(
            RecommendationReason::SharedSubject("x".into()).type_key(),
            "shared_subject"
        );
        assert_eq!(
            RecommendationReason::SamePublisher("x".into()).type_key(),
            "same_publisher"
        );
        assert_eq!(
            RecommendationReason::ClosePeriod(1942, 1947).type_key(),
            "close_period"
        );
        assert_eq!(
            RecommendationReason::ClosePeriod(1942, 1947).value(),
            "1942 / 1947"
        );
        assert_eq!(RecommendationReason::HighlyRated.type_key(), "highly_rated");
        assert_eq!(
            RecommendationReason::InReadingPile.type_key(),
            "in_reading_pile"
        );
    }
}
