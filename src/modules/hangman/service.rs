//! Hangman service -- business logic
//!
//! Handles difficulty configuration, title filtering, Unicode normalization,
//! scoring formula, and game lifecycle. All DB access goes through HangmanRepository trait.

use std::collections::HashSet;

use chrono::Local;
use rand::seq::SliceRandom;
use rand::thread_rng;

use super::domain::{
    DomainError, HangmanBook, HangmanChar, HangmanRepository, HangmanResult, HangmanScore,
    HangmanSetup,
};

/// Difficulty levels for the hangman game
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HangmanDifficulty {
    Easy,
    Medium,
    Hard,
}

impl HangmanDifficulty {
    /// Maximum wrong guesses before game over
    pub fn max_errors(&self) -> u8 {
        6
    }

    /// Number of hints available
    pub fn hints_available(&self) -> u8 {
        match self {
            Self::Easy => 2,
            Self::Medium => 1,
            Self::Hard => 0,
        }
    }

    /// Maximum allowed time in seconds
    pub fn max_time_seconds(&self) -> f64 {
        match self {
            Self::Easy => 300.0,
            Self::Medium => 180.0,
            Self::Hard => 120.0,
        }
    }

    /// Score multiplier
    pub fn multiplier(&self) -> f64 {
        match self {
            Self::Easy => 1.0,
            Self::Medium => 1.5,
            Self::Hard => 2.5,
        }
    }

    /// All difficulty levels in order
    pub fn all() -> &'static [HangmanDifficulty] {
        &[Self::Easy, Self::Medium, Self::Hard]
    }

    /// Parse from string (case-insensitive)
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        match s.to_lowercase().as_str() {
            "easy" => Ok(Self::Easy),
            "medium" => Ok(Self::Medium),
            "hard" => Ok(Self::Hard),
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
        }
    }
}

/// Minimum number of valid titles required to play
pub const MIN_BOOKS_REQUIRED: usize = 10;

/// Library size multiplier for scoring.
///
/// Larger libraries are harder (more obscure titles), so they get a bonus.
/// Smaller libraries are easier (player knows all titles), so a slight penalty.
pub fn library_multiplier(valid_books_count: usize) -> f64 {
    match valid_books_count {
        0..=19 => 0.7,
        20..=49 => 0.85,
        50..=99 => 1.0,
        _ => 1.15,
    }
}

/// Normalize a character to its base form for matching.
///
/// Letters are lowercased and stripped of combining marks (accents).
/// Digits pass through unchanged. Common ligatures are mapped to their base.
pub fn normalize_char(c: char) -> char {
    match c {
        '\u{00E6}' | '\u{00C6}' => 'a', // ae/AE
        '\u{0153}' | '\u{0152}' => 'o', // oe/OE
        '\u{00DF}' => 's',              // ss (eszett)
        '\u{00F0}' | '\u{00D0}' => 'd', // eth
        '\u{00FE}' | '\u{00DE}' => 't', // thorn
        '\u{00F8}' | '\u{00D8}' => 'o', // o-stroke
        _ if c.is_ascii_digit() => c,
        _ if c.is_alphabetic() => {
            // NFD decomposition: strip combining marks
            let decomposed: String = c
                .to_lowercase()
                .flat_map(|ch| {
                    use std::iter;
                    let mut nfd = String::new();
                    for nc in unicode_normalization::UnicodeNormalization::nfd(
                        iter::once(ch).collect::<String>().as_str(),
                    ) {
                        nfd.push(nc);
                    }
                    nfd.chars().collect::<Vec<_>>()
                })
                .filter(|ch| !unicode_normalization::char::is_combining_mark(*ch))
                .collect();
            decomposed.chars().next().unwrap_or(c)
        }
        _ => c,
    }
}

/// Check whether a title is valid for the hangman game.
///
/// Criteria:
///   1. At least 4 guessable characters (letters + digits)
///   2. At most 80 total characters
///   3. At least 3 distinct base characters
pub fn is_valid_title(title: &str) -> bool {
    let guessable_count = title.chars().filter(|c| c.is_alphanumeric()).count();
    let unique_base_chars: HashSet<char> = title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(normalize_char)
        .collect();

    guessable_count >= 4 && title.len() <= 80 && unique_base_chars.len() >= 3
}

/// Build the display mask for a title.
///
/// Letters and digits are guessable (hidden). Spaces, punctuation, etc. are revealed.
fn build_display(title: &str) -> Vec<HangmanChar> {
    title
        .chars()
        .map(|c| {
            let is_guessable = c.is_alphanumeric();
            HangmanChar {
                character: c,
                base_char: if is_guessable { normalize_char(c) } else { c },
                revealed: !is_guessable,
                is_guessable,
            }
        })
        .collect()
}

/// Get available difficulties based on how many valid titles exist
pub async fn available_difficulties(
    repo: &dyn HangmanRepository,
) -> Result<Vec<HangmanDifficulty>, DomainError> {
    let books = repo.find_eligible_books().await?;
    let valid_count = books.iter().filter(|b| is_valid_title(&b.title)).count();

    if valid_count >= MIN_BOOKS_REQUIRED {
        Ok(HangmanDifficulty::all().to_vec())
    } else {
        Ok(vec![])
    }
}

/// Set up a new game: pick a random valid title, avoiding recently played ones
pub async fn setup_game(
    repo: &dyn HangmanRepository,
    difficulty: HangmanDifficulty,
) -> Result<HangmanSetup, DomainError> {
    let books = repo.find_eligible_books().await?;
    let valid_books: Vec<&HangmanBook> =
        books.iter().filter(|b| is_valid_title(&b.title)).collect();

    if valid_books.len() < MIN_BOOKS_REQUIRED {
        return Err(DomainError::Validation(format!(
            "Not enough valid titles: need {}, have {}",
            MIN_BOOKS_REQUIRED,
            valid_books.len()
        )));
    }

    // Exclude recently played titles (up to half the pool, so we always have choices)
    let recent_limit = (valid_books.len() / 2).min(20) as u32;
    let recent_ids: HashSet<i32> = repo
        .get_recent_book_ids(recent_limit)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let fresh_books: Vec<&&HangmanBook> = valid_books
        .iter()
        .filter(|b| !recent_ids.contains(&b.book_id))
        .collect();

    let mut rng = thread_rng();
    // Pick from fresh books if available, otherwise fall back to all valid books
    let book = if fresh_books.is_empty() {
        valid_books.choose(&mut rng).unwrap()
    } else {
        **fresh_books.choose(&mut rng).unwrap()
    };

    Ok(HangmanSetup {
        book_id: book.book_id,
        title: book.title.clone(),
        display: build_display(&book.title),
        author: book.author.clone(),
        cover_url: book.cover_url.clone(),
        max_errors: difficulty.max_errors(),
        hints_available: difficulty.hints_available(),
        difficulty: difficulty.as_str().to_string(),
    })
}

/// Compute the normalized score for a completed game.
///
/// Formula:
///   - Lost game: 0
///   - Won game:
///     time_score = max(0, (max_time - elapsed) / max_time) * 1000
///     error_penalty = errors * 100
///     hint_penalty = hints_used * 200
///     raw_score = max(0, time_score - error_penalty - hint_penalty)
///     normalized_score = raw_score * difficulty_multiplier * library_multiplier
pub fn compute_score(result: &HangmanResult, valid_books_count: usize) -> Result<f64, DomainError> {
    if !result.won {
        return Ok(0.0);
    }

    let difficulty = HangmanDifficulty::parse(&result.difficulty)?;
    let max_time = difficulty.max_time_seconds();

    let time_score = ((max_time - result.elapsed_seconds) / max_time * 1000.0).max(0.0);
    let error_penalty = result.errors as f64 * 100.0;
    let hint_penalty = result.hints_used as f64 * 200.0;

    let raw_score = (time_score - error_penalty - hint_penalty).max(0.0);
    let lib_mult = library_multiplier(valid_books_count);
    let normalized_score = raw_score * difficulty.multiplier() * lib_mult;

    Ok(normalized_score)
}

/// Finish a game: compute score (with library size bonus), persist it, return the saved score
pub async fn finish_game(
    repo: &dyn HangmanRepository,
    result: HangmanResult,
) -> Result<HangmanScore, DomainError> {
    // Count valid titles for library size multiplier
    let books = repo.find_eligible_books().await?;
    let valid_count = books.iter().filter(|b| is_valid_title(&b.title)).count();
    let normalized_score = compute_score(&result, valid_count)?;
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let score = HangmanScore {
        id: None,
        book_id: result.book_id,
        difficulty: result.difficulty,
        elapsed_seconds: result.elapsed_seconds,
        errors: result.errors,
        hints_used: result.hints_used,
        won: result.won,
        normalized_score,
        played_at: now,
    };

    repo.save_score(score).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(
        difficulty: &str,
        elapsed: f64,
        errors: i32,
        hints: i32,
        won: bool,
    ) -> HangmanResult {
        HangmanResult {
            book_id: 1,
            difficulty: difficulty.to_string(),
            elapsed_seconds: elapsed,
            errors,
            hints_used: hints,
            won,
        }
    }

    // ── Scoring tests ──────────────────────────────────────────────

    #[test]
    fn test_score_perfect_easy() {
        let r = make_result("easy", 0.0, 0, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_perfect_medium() {
        let r = make_result("medium", 0.0, 0, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 1500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_perfect_hard() {
        let r = make_result("hard", 0.0, 0, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 2500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_lost_game() {
        let r = make_result("easy", 30.0, 6, 0, false);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_with_errors() {
        // Easy, 0s, 2 errors, 0 hints: (1000 - 200) * 1.0 = 800
        let r = make_result("easy", 0.0, 2, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_with_hints() {
        // Easy, 0s, 0 errors, 1 hint: (1000 - 200) * 1.0 = 800
        let r = make_result("easy", 0.0, 0, 1, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_errors_and_hints() {
        // Easy, 60s, 2 errors, 1 hint: (800 - 200 - 200) * 1.0 = 400
        let r = make_result("easy", 60.0, 2, 1, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 400.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_clamps_to_zero() {
        // Medium, 90s, 3 errors, 1 hint: (500 - 300 - 200) * 1.5 = 0
        let r = make_result("medium", 90.0, 3, 1, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_over_time() {
        let r = make_result("easy", 300.0, 0, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_way_over_time() {
        let r = make_result("easy", 600.0, 0, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_half_time_hard() {
        // Hard, 60s, 1 error, 0 hints: (500 - 100) * 2.5 = 1000
        let r = make_result("hard", 60.0, 1, 0, true);
        let s = compute_score(&r, 50).unwrap();
        assert!((s - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_invalid_difficulty() {
        let r = make_result("impossible", 0.0, 0, 0, true);
        assert!(compute_score(&r, 50).is_err());
    }

    // ── Difficulty tests ───────────────────────────────────────────

    #[test]
    fn test_difficulty_parse() {
        assert_eq!(
            HangmanDifficulty::parse("easy").unwrap(),
            HangmanDifficulty::Easy
        );
        assert_eq!(
            HangmanDifficulty::parse("HARD").unwrap(),
            HangmanDifficulty::Hard
        );
        assert!(HangmanDifficulty::parse("unknown").is_err());
    }

    #[test]
    fn test_difficulty_hints() {
        assert_eq!(HangmanDifficulty::Easy.hints_available(), 2);
        assert_eq!(HangmanDifficulty::Medium.hints_available(), 1);
        assert_eq!(HangmanDifficulty::Hard.hints_available(), 0);
    }

    #[test]
    fn test_difficulty_multiplier() {
        assert!((HangmanDifficulty::Easy.multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((HangmanDifficulty::Medium.multiplier() - 1.5).abs() < f64::EPSILON);
        assert!((HangmanDifficulty::Hard.multiplier() - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_difficulty_max_time() {
        assert!((HangmanDifficulty::Easy.max_time_seconds() - 300.0).abs() < f64::EPSILON);
        assert!((HangmanDifficulty::Medium.max_time_seconds() - 180.0).abs() < f64::EPSILON);
        assert!((HangmanDifficulty::Hard.max_time_seconds() - 120.0).abs() < f64::EPSILON);
    }

    // ── Normalization tests ────────────────────────────────────────

    #[test]
    fn test_normalize_basic_letters() {
        assert_eq!(normalize_char('A'), 'a');
        assert_eq!(normalize_char('z'), 'z');
    }

    #[test]
    fn test_normalize_accented_french() {
        assert_eq!(normalize_char('\u{00E9}'), 'e'); // e
        assert_eq!(normalize_char('\u{00E8}'), 'e'); // e
        assert_eq!(normalize_char('\u{00EA}'), 'e'); // e
        assert_eq!(normalize_char('\u{00E7}'), 'c'); // c
    }

    #[test]
    fn test_normalize_accented_spanish() {
        assert_eq!(normalize_char('\u{00F1}'), 'n'); // n
        assert_eq!(normalize_char('\u{00FC}'), 'u'); // u
    }

    #[test]
    fn test_normalize_accented_german() {
        assert_eq!(normalize_char('\u{00E4}'), 'a'); // a
        assert_eq!(normalize_char('\u{00F6}'), 'o'); // o
        assert_eq!(normalize_char('\u{00DC}'), 'u'); // U
    }

    #[test]
    fn test_normalize_ligatures() {
        assert_eq!(normalize_char('\u{0153}'), 'o'); // oe
        assert_eq!(normalize_char('\u{00E6}'), 'a'); // ae
        assert_eq!(normalize_char('\u{00DF}'), 's'); // ss
        assert_eq!(normalize_char('\u{00F8}'), 'o'); // o-stroke
    }

    #[test]
    fn test_normalize_digits() {
        assert_eq!(normalize_char('0'), '0');
        assert_eq!(normalize_char('9'), '9');
    }

    #[test]
    fn test_normalize_punctuation_passthrough() {
        assert_eq!(normalize_char(' '), ' ');
        assert_eq!(normalize_char('\''), '\'');
        assert_eq!(normalize_char('-'), '-');
    }

    // ── Title filtering tests ──────────────────────────────────────

    #[test]
    fn test_valid_title_standard() {
        assert!(is_valid_title("Dune"));
        assert!(is_valid_title("Les Miserables"));
        assert!(is_valid_title("L'Etranger"));
        assert!(is_valid_title("Fahrenheit 451"));
    }

    #[test]
    fn test_valid_title_digits_only() {
        assert!(is_valid_title("1984")); // 4 chars, 4 unique
    }

    #[test]
    fn test_invalid_title_too_short() {
        assert!(!is_valid_title("It")); // 2 chars
        assert!(!is_valid_title("Q")); // 1 char
    }

    #[test]
    fn test_invalid_title_too_few_unique() {
        assert!(!is_valid_title("SOS")); // 3 chars, 2 unique (S, O)
    }

    #[test]
    fn test_invalid_title_too_long() {
        let long_title = "A".repeat(81);
        assert!(!is_valid_title(&long_title));
    }

    #[test]
    fn test_valid_title_at_max_length() {
        let title = "Ab1".repeat(26) + "Cd"; // 80 chars, 4 unique base chars
        assert!(is_valid_title(&title));
    }

    // ── Display mask tests ─────────────────────────────────────────

    #[test]
    fn test_display_letters_hidden() {
        let display = build_display("Abc");
        assert!(display[0].is_guessable);
        assert!(!display[0].revealed);
        assert_eq!(display[0].character, 'A');
        assert_eq!(display[0].base_char, 'a');
    }

    #[test]
    fn test_display_space_visible() {
        let display = build_display("A B");
        assert!(!display[1].is_guessable);
        assert!(display[1].revealed);
    }

    #[test]
    fn test_display_punctuation_visible() {
        let display = build_display("L'X");
        assert!(!display[1].is_guessable); // apostrophe
        assert!(display[1].revealed);
    }

    #[test]
    fn test_display_digit_hidden() {
        let display = build_display("F451");
        assert!(display[1].is_guessable); // '4'
        assert!(!display[1].revealed);
        assert_eq!(display[1].base_char, '4');
    }

    #[test]
    fn test_display_accented_char() {
        let display = build_display("\u{00E9}"); // e
        assert!(display[0].is_guessable);
        assert_eq!(display[0].character, '\u{00E9}');
        assert_eq!(display[0].base_char, 'e');
    }

    // ── Library multiplier tests ───────────────────────────────────

    #[test]
    fn test_library_multiplier_small() {
        assert!((library_multiplier(10) - 0.7).abs() < f64::EPSILON);
        assert!((library_multiplier(19) - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_library_multiplier_medium() {
        assert!((library_multiplier(20) - 0.85).abs() < f64::EPSILON);
        assert!((library_multiplier(49) - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn test_library_multiplier_baseline() {
        assert!((library_multiplier(50) - 1.0).abs() < f64::EPSILON);
        assert!((library_multiplier(99) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_library_multiplier_large() {
        assert!((library_multiplier(100) - 1.15).abs() < f64::EPSILON);
        assert!((library_multiplier(500) - 1.15).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_with_library_multiplier_small() {
        // Perfect easy, 10 books: 1000 * 1.0 * 0.7 = 700
        let r = make_result("easy", 0.0, 0, 0, true);
        let s = compute_score(&r, 10).unwrap();
        assert!((s - 700.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_score_with_library_multiplier_large() {
        // Perfect easy, 100 books: 1000 * 1.0 * 1.15 = 1150
        let r = make_result("easy", 0.0, 0, 0, true);
        let s = compute_score(&r, 100).unwrap();
        assert!((s - 1150.0).abs() < f64::EPSILON);
    }
}
