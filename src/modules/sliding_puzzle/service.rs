//! Sliding Puzzle service - business logic
//!
//! Handles difficulty configuration, board generation (reverse-shuffle),
//! scoring formula, and game lifecycle. All DB access goes through
//! SlidingPuzzleRepository trait.

use chrono::Local;
use rand::seq::SliceRandom;
use rand::thread_rng;

use super::domain::{DomainError, PuzzleBoard, PuzzleResult, PuzzleScore, SlidingPuzzleRepository};

/// Difficulty levels for the sliding puzzle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PuzzleDifficulty {
    Easy,
    Medium,
    Hard,
}

impl PuzzleDifficulty {
    /// Grid size (N x N)
    pub fn grid_size(&self) -> u8 {
        match self {
            Self::Easy => 3,
            Self::Medium => 4,
            Self::Hard => 5,
        }
    }

    /// Number of random moves used to shuffle the board.
    /// Determines puzzle complexity. Separate from par_moves (scoring target).
    pub fn shuffle_moves(&self) -> u32 {
        match self {
            Self::Easy => 40,
            Self::Medium => 100,
            Self::Hard => 200,
        }
    }

    /// Par moves: the target number of moves for scoring.
    /// Set generously so a casual player can score above 0.
    /// A good player beats par; a perfect player approaches optimal (~shuffle/2).
    pub fn par_moves(&self) -> u32 {
        match self {
            Self::Easy => 120,   // 3x3: optimal ~20-30, casual ~80-150
            Self::Medium => 300, // 4x4: optimal ~80-150, casual ~200-400
            Self::Hard => 600,   // 5x5: optimal ~150-300, casual ~400-800
        }
    }

    /// Maximum allowed time in seconds (for scoring, not a hard cutoff).
    /// Generous enough that a casual player still gets partial time credit.
    pub fn max_time_seconds(&self) -> f64 {
        match self {
            Self::Easy => 300.0,   // 5 minutes
            Self::Medium => 600.0, // 10 minutes
            Self::Hard => 900.0,   // 15 minutes
        }
    }

    /// Score multiplier
    pub fn multiplier(&self) -> f64 {
        match self {
            Self::Easy => 1.0,
            Self::Medium => 1.8,
            Self::Hard => 3.0,
        }
    }

    /// Minimum books with covers needed for this difficulty
    pub fn min_books_required(&self) -> usize {
        1
    }

    /// All difficulty levels in order
    pub fn all() -> &'static [PuzzleDifficulty] {
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

/// Get valid neighbor indices for a given position in an N x N grid
pub fn get_neighbors(index: usize, grid_size: u8) -> Vec<usize> {
    let n = grid_size as usize;
    let row = index / n;
    let col = index % n;
    let mut neighbors = Vec::with_capacity(4);

    if row > 0 {
        neighbors.push(index - n); // up
    }
    if row < n - 1 {
        neighbors.push(index + n); // down
    }
    if col > 0 {
        neighbors.push(index - 1); // left
    }
    if col < n - 1 {
        neighbors.push(index + 1); // right
    }

    neighbors
}

/// Generate a shuffled board using reverse-shuffle from the solved state.
///
/// Starts from `[1, 2, ..., N*N-1, 0]` (0 = empty), then applies
/// `num_moves` random valid swaps, excluding immediate back-tracking.
/// Solvability is guaranteed by construction.
pub fn generate_board(grid_size: u8, num_moves: u32) -> (Vec<u8>, usize) {
    let n = grid_size as usize;
    let total = n * n;

    // Solved state: [1, 2, ..., N*N-1, 0]
    let mut tiles: Vec<u8> = (1..total as u8).collect();
    tiles.push(0);

    let mut empty = total - 1;
    let mut prev_empty: Option<usize> = None;
    let mut rng = thread_rng();

    for _ in 0..num_moves {
        let neighbors = get_neighbors(empty, grid_size);
        // Filter out the previous empty position to avoid immediate undo
        let candidates: Vec<usize> = neighbors
            .into_iter()
            .filter(|&n| prev_empty != Some(n))
            .collect();

        if let Some(&chosen) = candidates.choose(&mut rng) {
            tiles.swap(empty, chosen);
            prev_empty = Some(empty);
            empty = chosen;
        }
    }

    (tiles, empty)
}

/// Get available difficulties based on how many books have covers
pub async fn available_difficulties(
    repo: &dyn SlidingPuzzleRepository,
) -> Result<Vec<PuzzleDifficulty>, DomainError> {
    let books = repo.find_books_with_covers().await?;
    let count = books.len();

    let available = PuzzleDifficulty::all()
        .iter()
        .filter(|d| count >= d.min_books_required())
        .copied()
        .collect();

    Ok(available)
}

/// Upgrade a cover URL to an appropriate resolution based on grid size.
///
/// For 3x3 grids (tiles ~100px each), we need higher resolution:
/// - OpenLibrary: upgrade to `-L.jpg` (~325x500)
/// - Google Books: upgrade to `zoom=3`
///
/// For 4x4+ grids (tiles ~60-75px each), medium resolution suffices:
/// - OpenLibrary: ensure at least `-M.jpg` (~180x270)
/// - Google Books: ensure at least `zoom=1`
///
/// This keeps downloads small on slow networks (2G/3G) while avoiding
/// pixelated tiles on larger grids.
fn upgrade_cover_url(url: &str, grid_size: usize) -> String {
    let need_large = grid_size <= 3;

    // OpenLibrary covers: https://covers.openlibrary.org/b/id/12345-M.jpg
    if url.contains("covers.openlibrary.org") {
        if need_large {
            return url.replace("-S.jpg", "-L.jpg").replace("-M.jpg", "-L.jpg");
        }
        // For 4x4+, just ensure we have at least -M (upgrade -S only)
        return url.replace("-S.jpg", "-M.jpg");
    }

    // Google Books thumbnails
    if url.contains("books.google") || url.contains("googleapis.com/books") {
        if need_large {
            if url.contains("zoom=1") {
                return url.replace("zoom=1", "zoom=3");
            }
            if url.contains("zoom=0") {
                return url.replace("zoom=0", "zoom=3");
            }
        } else {
            // For 4x4+, just ensure at least zoom=1 (upgrade zoom=0 only)
            if url.contains("zoom=0") {
                return url.replace("zoom=0", "zoom=1");
            }
        }
    }

    url.to_string()
}

/// Set up a new puzzle game: pick a random book, generate a shuffled board
pub async fn setup_game(
    repo: &dyn SlidingPuzzleRepository,
    difficulty: PuzzleDifficulty,
) -> Result<PuzzleBoard, DomainError> {
    let books = repo.find_books_with_covers().await?;

    if books.len() < difficulty.min_books_required() {
        return Err(DomainError::Validation(format!(
            "Not enough books with covers: need {}, have {}",
            difficulty.min_books_required(),
            books.len()
        )));
    }

    let mut rng = thread_rng();
    let book = books.choose(&mut rng).unwrap().clone();

    let grid_size = difficulty.grid_size();
    let par_moves = difficulty.par_moves();
    let (tiles, empty_index) = generate_board(grid_size, difficulty.shuffle_moves());

    Ok(PuzzleBoard {
        book_id: book.book_id,
        title: book.title,
        cover_url: upgrade_cover_url(&book.cover_url, grid_size as usize),
        grid_size,
        tiles,
        empty_index,
        par_moves,
    })
}

/// Compute the normalized score for a completed puzzle
///
/// Formula (60% moves, 40% time):
///   move_efficiency = max(0, (par_moves - actual_moves) / par_moves)
///   time_score      = max(0, (max_time - elapsed) / max_time)
///   raw_score       = move_efficiency * 600 + time_score * 400
///   normalized      = raw_score * difficulty_multiplier
pub fn compute_score(result: &PuzzleResult) -> Result<f64, DomainError> {
    let difficulty = PuzzleDifficulty::parse(&result.difficulty)?;
    let max_time = difficulty.max_time_seconds();
    // Use canonical par_moves from difficulty (authoritative), not the client-supplied value
    let par = difficulty.par_moves() as f64;

    let move_efficiency = ((par - result.move_count as f64) / par).max(0.0);
    let time_score = ((max_time - result.elapsed_seconds) / max_time).max(0.0);
    let raw_score = move_efficiency * 600.0 + time_score * 400.0;
    let normalized = raw_score * difficulty.multiplier();

    Ok(normalized)
}

/// Finish a game: compute score, persist it, return the saved score
pub async fn finish_game(
    repo: &dyn SlidingPuzzleRepository,
    result: PuzzleResult,
) -> Result<PuzzleScore, DomainError> {
    let difficulty = PuzzleDifficulty::parse(&result.difficulty)?;
    let normalized_score = compute_score(&result)?;
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let score = PuzzleScore {
        id: None,
        difficulty: result.difficulty,
        grid_size: result.grid_size as i32,
        elapsed_seconds: result.elapsed_seconds,
        move_count: result.move_count as i32,
        par_moves: difficulty.par_moves() as i32,
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
        moves: u32,
        par: u32,
        grid_size: u8,
    ) -> PuzzleResult {
        PuzzleResult {
            difficulty: difficulty.to_string(),
            grid_size,
            elapsed_seconds: elapsed,
            move_count: moves,
            par_moves: par,
        }
    }

    // ── Scoring tests ─────────────────────────────────────────────────────

    #[test]
    fn test_compute_score_perfect_easy() {
        // 0 moves, 0 seconds on easy (par=120, max_time=300): move_eff=1.0, time=1.0
        // raw = 1.0*600 + 1.0*400 = 1000, * 1.0 = 1000
        let result = make_result("easy", 0.0, 0, 120, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_perfect_medium() {
        // 0 moves, 0 seconds on medium (par=300, max_time=600): raw=1000, * 1.8 = 1800
        let result = make_result("medium", 0.0, 0, 300, 4);
        let score = compute_score(&result).unwrap();
        assert!((score - 1800.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_perfect_hard() {
        // 0 moves, 0 seconds on hard (par=600, max_time=900): raw=1000, * 3.0 = 3000
        let result = make_result("hard", 0.0, 0, 600, 5);
        let score = compute_score(&result).unwrap();
        assert!((score - 3000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_at_par_moves_zero_time() {
        // par moves exactly (120), 0 seconds: move_eff=0, time=1.0
        // raw = 0*600 + 1.0*400 = 400, * 1.0 = 400
        let result = make_result("easy", 0.0, 120, 120, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 400.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_at_max_time_zero_moves() {
        // 0 moves, max time (300s easy, par=120): move_eff=1.0, time=0
        // raw = 1.0*600 + 0*400 = 600, * 1.0 = 600
        let result = make_result("easy", 300.0, 0, 120, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 600.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_half_moves_half_time() {
        // 60 of 120 moves, 150 of 300 seconds on easy
        // move_eff = (120-60)/120 = 0.5, time = (300-150)/300 = 0.5
        // raw = 0.5*600 + 0.5*400 = 500, * 1.0 = 500
        let result = make_result("easy", 150.0, 60, 120, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_over_par_and_over_time() {
        // More moves than par (250>120), more time than max (400>300): both clamp to 0
        let result = make_result("easy", 400.0, 250, 120, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_way_over_par_clamps() {
        // Tons of moves (200>120), no time used
        // move_eff = max(0, (120-200)/120) = 0, time=1.0
        // raw = 0 + 400 = 400
        let result = make_result("easy", 0.0, 200, 120, 3);
        let score = compute_score(&result).unwrap();
        assert!((score - 400.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_medium_multiplier() {
        // Half efficiency on medium (par=300, max_time=600s): raw=500, * 1.8 = 900
        let result = make_result("medium", 300.0, 150, 300, 4);
        let score = compute_score(&result).unwrap();
        assert!((score - 900.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_hard_multiplier() {
        // Half efficiency on hard (par=600, max_time=900s): raw=500, * 3.0 = 1500
        let result = make_result("hard", 450.0, 300, 600, 5);
        let score = compute_score(&result).unwrap();
        assert!((score - 1500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_score_invalid_difficulty() {
        let result = make_result("impossible", 0.0, 0, 120, 3);
        assert!(compute_score(&result).is_err());
    }

    // ── Difficulty tests ──────────────────────────────────────────────────

    #[test]
    fn test_difficulty_parse() {
        assert_eq!(
            PuzzleDifficulty::parse("easy").unwrap(),
            PuzzleDifficulty::Easy
        );
        assert_eq!(
            PuzzleDifficulty::parse("MEDIUM").unwrap(),
            PuzzleDifficulty::Medium
        );
        assert_eq!(
            PuzzleDifficulty::parse("Hard").unwrap(),
            PuzzleDifficulty::Hard
        );
        assert!(PuzzleDifficulty::parse("unknown").is_err());
    }

    #[test]
    fn test_difficulty_grid_size() {
        assert_eq!(PuzzleDifficulty::Easy.grid_size(), 3);
        assert_eq!(PuzzleDifficulty::Medium.grid_size(), 4);
        assert_eq!(PuzzleDifficulty::Hard.grid_size(), 5);
    }

    #[test]
    fn test_difficulty_shuffle_moves() {
        assert_eq!(PuzzleDifficulty::Easy.shuffle_moves(), 40);
        assert_eq!(PuzzleDifficulty::Medium.shuffle_moves(), 100);
        assert_eq!(PuzzleDifficulty::Hard.shuffle_moves(), 200);
    }

    #[test]
    fn test_difficulty_par_moves() {
        assert_eq!(PuzzleDifficulty::Easy.par_moves(), 120);
        assert_eq!(PuzzleDifficulty::Medium.par_moves(), 300);
        assert_eq!(PuzzleDifficulty::Hard.par_moves(), 600);
    }

    #[test]
    fn test_difficulty_max_time() {
        assert!((PuzzleDifficulty::Easy.max_time_seconds() - 300.0).abs() < f64::EPSILON);
        assert!((PuzzleDifficulty::Medium.max_time_seconds() - 600.0).abs() < f64::EPSILON);
        assert!((PuzzleDifficulty::Hard.max_time_seconds() - 900.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_difficulty_multiplier() {
        assert!((PuzzleDifficulty::Easy.multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((PuzzleDifficulty::Medium.multiplier() - 1.8).abs() < f64::EPSILON);
        assert!((PuzzleDifficulty::Hard.multiplier() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_difficulty_as_str_roundtrip() {
        for d in PuzzleDifficulty::all() {
            let s = d.as_str();
            let parsed = PuzzleDifficulty::parse(s).unwrap();
            assert_eq!(*d, parsed);
        }
    }

    // ── Board generation tests ────────────────────────────────────────────

    #[test]
    fn test_generate_board_3x3_valid_tiles() {
        let (tiles, empty) = generate_board(3, 40);
        assert_eq!(tiles.len(), 9);
        assert_eq!(tiles[empty], 0);

        // All values 0..=8 present exactly once
        let mut sorted = tiles.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_generate_board_4x4_valid_tiles() {
        let (tiles, empty) = generate_board(4, 100);
        assert_eq!(tiles.len(), 16);
        assert_eq!(tiles[empty], 0);

        let mut sorted = tiles.clone();
        sorted.sort();
        let expected: Vec<u8> = (0..16).collect();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn test_generate_board_5x5_valid_tiles() {
        let (tiles, empty) = generate_board(5, 200);
        assert_eq!(tiles.len(), 25);
        assert_eq!(tiles[empty], 0);

        let mut sorted = tiles.clone();
        sorted.sort();
        let expected: Vec<u8> = (0..25).collect();
        assert_eq!(sorted, expected);
    }

    #[test]
    fn test_generate_board_is_shuffled() {
        // With 40 moves, the board should differ from solved state
        let solved_3x3: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 0];
        let (tiles, _) = generate_board(3, 40);
        // Extremely unlikely to get solved state back after 40 moves
        assert_ne!(tiles, solved_3x3);
    }

    #[test]
    fn test_generate_board_zero_moves_is_solved() {
        let (tiles, empty) = generate_board(3, 0);
        assert_eq!(tiles, vec![1, 2, 3, 4, 5, 6, 7, 8, 0]);
        assert_eq!(empty, 8);
    }

    // ── Neighbor tests ────────────────────────────────────────────────────

    #[test]
    fn test_get_neighbors_corner_top_left() {
        // Position 0 in 3x3: right (1) and down (3)
        let n = get_neighbors(0, 3);
        assert_eq!(n.len(), 2);
        assert!(n.contains(&1));
        assert!(n.contains(&3));
    }

    #[test]
    fn test_get_neighbors_corner_bottom_right() {
        // Position 8 in 3x3: left (7) and up (5)
        let n = get_neighbors(8, 3);
        assert_eq!(n.len(), 2);
        assert!(n.contains(&7));
        assert!(n.contains(&5));
    }

    #[test]
    fn test_get_neighbors_center() {
        // Position 4 in 3x3: up (1), down (7), left (3), right (5)
        let n = get_neighbors(4, 3);
        assert_eq!(n.len(), 4);
        assert!(n.contains(&1));
        assert!(n.contains(&7));
        assert!(n.contains(&3));
        assert!(n.contains(&5));
    }

    #[test]
    fn test_get_neighbors_edge() {
        // Position 1 in 3x3 (top edge, middle): up=none, down (4), left (0), right (2)
        let n = get_neighbors(1, 3);
        assert_eq!(n.len(), 3);
        assert!(n.contains(&0));
        assert!(n.contains(&2));
        assert!(n.contains(&4));
    }

    #[test]
    fn test_get_neighbors_4x4_center() {
        // Position 5 in 4x4 (row 1, col 1): up (1), down (9), left (4), right (6)
        let n = get_neighbors(5, 4);
        assert_eq!(n.len(), 4);
        assert!(n.contains(&1));
        assert!(n.contains(&9));
        assert!(n.contains(&4));
        assert!(n.contains(&6));
    }

    #[test]
    fn test_get_neighbors_5x5_bottom_left() {
        // Position 20 in 5x5 (row 4, col 0): up (15), right (21)
        let n = get_neighbors(20, 5);
        assert_eq!(n.len(), 2);
        assert!(n.contains(&15));
        assert!(n.contains(&21));
    }

    // ── Cover URL upgrade tests ───────────────────────────────────────────

    // 3x3 grid: upgrade to large resolution
    #[test]
    fn test_upgrade_openlibrary_medium_to_large_3x3() {
        let url = "https://covers.openlibrary.org/b/id/8243022-M.jpg";
        assert_eq!(
            upgrade_cover_url(url, 3),
            "https://covers.openlibrary.org/b/id/8243022-L.jpg"
        );
    }

    #[test]
    fn test_upgrade_openlibrary_small_to_large_3x3() {
        let url = "https://covers.openlibrary.org/b/id/8243022-S.jpg";
        assert_eq!(
            upgrade_cover_url(url, 3),
            "https://covers.openlibrary.org/b/id/8243022-L.jpg"
        );
    }

    #[test]
    fn test_upgrade_openlibrary_large_unchanged_3x3() {
        let url = "https://covers.openlibrary.org/b/id/8243022-L.jpg";
        assert_eq!(upgrade_cover_url(url, 3), url);
    }

    #[test]
    fn test_upgrade_google_books_zoom_3x3() {
        let url = "https://books.google.com/content?id=abc&zoom=1";
        assert_eq!(
            upgrade_cover_url(url, 3),
            "https://books.google.com/content?id=abc&zoom=3"
        );
    }

    // 4x4+ grid: keep medium resolution (save bandwidth)
    #[test]
    fn test_upgrade_openlibrary_medium_unchanged_4x4() {
        let url = "https://covers.openlibrary.org/b/id/8243022-M.jpg";
        assert_eq!(upgrade_cover_url(url, 4), url);
    }

    #[test]
    fn test_upgrade_openlibrary_small_to_medium_4x4() {
        let url = "https://covers.openlibrary.org/b/id/8243022-S.jpg";
        assert_eq!(
            upgrade_cover_url(url, 4),
            "https://covers.openlibrary.org/b/id/8243022-M.jpg"
        );
    }

    #[test]
    fn test_upgrade_google_books_zoom1_unchanged_5x5() {
        let url = "https://books.google.com/content?id=abc&zoom=1";
        assert_eq!(upgrade_cover_url(url, 5), url);
    }

    #[test]
    fn test_upgrade_google_books_zoom0_to_zoom1_4x4() {
        let url = "https://books.google.com/content?id=abc&zoom=0";
        assert_eq!(
            upgrade_cover_url(url, 4),
            "https://books.google.com/content?id=abc&zoom=1"
        );
    }

    #[test]
    fn test_upgrade_other_url_unchanged() {
        let url = "https://inventaire.io/img/entities/abc123";
        assert_eq!(upgrade_cover_url(url, 3), url);
        assert_eq!(upgrade_cover_url(url, 5), url);
    }
}
