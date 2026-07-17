// Extension games: memory, sliding puzzle, hangman, plus the colocated peers_relay_debug_info.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Memory Game (FFI) ============

/// A card in the memory game (FFI-safe)
pub struct FrbMemoryCard {
    pub book_id: String,
    pub title: String,
    pub cover_url: String,
}

/// A saved memory game score (FFI-safe)
pub struct FrbMemoryScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub pairs_count: i32,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub normalized_score: f64,
    pub played_at: String,
    /// Achievements unlocked after this game (empty if none)
    pub new_achievements: Vec<String>,
}

/// A leaderboard entry (FFI-safe)
pub struct FrbMemoryLeaderboardEntry {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
    /// True if this entry is the local user (not a peer)
    pub is_self: bool,
}

/// Get available difficulty levels based on books with covers
pub async fn memory_game_available_difficulties() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let difficulties = crate::modules::memory_game::service::available_difficulties(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(difficulties
        .iter()
        .map(|d| d.as_str().to_string())
        .collect())
}

/// Set up a new memory game with the given difficulty
pub async fn memory_game_setup(difficulty: String) -> Result<Vec<FrbMemoryCard>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let diff = crate::modules::memory_game::service::MemoryDifficulty::parse(&difficulty)
        .map_err(|e| e.to_string())?;
    let cards = crate::modules::memory_game::service::setup_game(&repo, diff)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cards
        .into_iter()
        .map(|c| FrbMemoryCard {
            book_id: c.book_id,
            title: c.title,
            cover_url: c.cover_url,
        })
        .collect())
}

/// Submit a completed game and get the score back
pub async fn memory_game_finish(
    difficulty: String,
    elapsed_seconds: f64,
    errors: i32,
    pairs_count: i32,
) -> Result<FrbMemoryScore, String> {
    let db = db().ok_or("Database not initialized")?;
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let result = crate::modules::memory_game::domain::MemoryGameResult {
        difficulty,
        elapsed_seconds,
        errors,
        pairs_count,
    };
    let score = crate::modules::memory_game::service::finish_game(&game_repo, result)
        .await
        .map_err(|e| e.to_string())?;

    // Check achievements after game completion
    let new_achievements = {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        let puzzle_repo =
            crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
        let hangman_repo =
            crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
        crate::services::gamification_service::check_and_unlock_achievements(
            &gamification_repo,
            &game_repo,
            Some(&puzzle_repo),
            Some(&hangman_repo),
        )
        .await
        .unwrap_or_default()
    };

    Ok(FrbMemoryScore {
        id: score.id,
        difficulty: score.difficulty,
        pairs_count: score.pairs_count,
        elapsed_seconds: score.elapsed_seconds,
        errors: score.errors,
        normalized_score: score.normalized_score,
        played_at: score.played_at,
        new_achievements,
    })
}

/// Get top memory game scores
pub async fn memory_game_top_scores() -> Result<Vec<FrbMemoryScore>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    use crate::modules::memory_game::domain::MemoryGameRepository;
    let scores = repo.get_top_scores(10).await.map_err(|e| e.to_string())?;
    Ok(scores
        .into_iter()
        .map(|s| FrbMemoryScore {
            id: s.id,
            difficulty: s.difficulty,
            pairs_count: s.pairs_count,
            elapsed_seconds: s.elapsed_seconds,
            errors: s.errors,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
            new_achievements: vec![],
        })
        .collect())
}

/// Get leaderboard (peer scores + local user's best)
pub async fn memory_game_leaderboard() -> Result<Vec<FrbMemoryLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    use crate::modules::memory_game::domain::MemoryGameRepository;

    // Peer scores
    let peer_scores = game_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbMemoryLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbMemoryLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score PER DIFFICULTY
    {
        use sea_orm::{ConnectionTrait, Statement};
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT difficulty, MAX(normalized_score) as best, played_at FROM memory_game_scores GROUP BY difficulty".to_owned(),
            ))
            .await
            .unwrap_or_default();

        for row in rows {
            if let (Ok(difficulty), Ok(best_score), Ok(played_at)) = (
                row.try_get::<String>("", "difficulty"),
                row.try_get::<f64>("", "best"),
                row.try_get::<String>("", "played_at"),
            ) {
                entries.push(FrbMemoryLeaderboardEntry {
                    peer_id: 0,
                    library_name: library_name.clone(),
                    best_score,
                    difficulty,
                    played_at,
                    is_self: true,
                });
            }
        }
    }

    // Sort by best_score descending
    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

/// Return a debug summary of all peers and their relay credential state.
///
/// Used to diagnose leaderboard relay issues (ADR-022). Returns one line per peer
/// with name, connection_status, key_exchange_done, and whether relay credentials
/// are present. Call from Flutter and log with debugPrint.
pub async fn peers_relay_debug_info() -> Result<String, String> {
    use sea_orm::EntityTrait;
    let db = db().ok_or("Database not initialized")?;
    let peers = crate::models::peer::Entity::find()
        .all(db)
        .await
        .map_err(|e| e.to_string())?;
    let mut lines = vec![format!("Total peers: {}", peers.len())];
    for p in &peers {
        lines.push(format!(
            "  [{status}] '{name}' kx={kx} relay_url={ru} mailbox={mb} write_token={wt}",
            status = p.connection_status,
            name = p.name,
            kx = p.key_exchange_done,
            ru = p.relay_url.is_some(),
            mb = p.mailbox_id.is_some(),
            wt = p.relay_write_token.is_some(),
        ));
    }
    Ok(lines.join("\n"))
}

/// Reset all local memory game scores.
pub async fn memory_game_reset_scores() -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::memory_game::domain::MemoryGameRepository;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    repo.delete_all_scores().await.map_err(|e| e.to_string())
}

/// Reset all local sliding puzzle scores.
pub async fn puzzle_game_reset_scores() -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    repo.delete_all_scores().await.map_err(|e| e.to_string())
}

/// Reset all local hangman scores.
pub async fn hangman_reset_scores() -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::hangman::domain::HangmanRepository;
    let repo = crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    repo.delete_all_scores().await.map_err(|e| e.to_string())
}

/// Refresh ALL leaderboard caches (memory, puzzle, hangman, gamification) in one pass.
///
/// A single relay round-trip per peer populates all game caches. When `skip_direct`
/// is true, Phase 1 direct HTTP is skipped (use on cellular where LAN peers are
/// unreachable). Called by Flutter at startup (pre-warm) and by per-game refresh.
pub async fn refresh_all_leaderboards(skip_direct: bool) -> Result<(), String> {
    if let Some(state) = global_app_state() {
        crate::utils::leaderboard_relay::sync_all_leaderboards(state, skip_direct).await;
    }
    Ok(())
}

/// Refresh the network memory game leaderboard by syncing with all accepted peers.
/// Uses the unified sync that populates all game caches in one relay pass.
pub async fn memory_game_refresh_leaderboard() -> Result<Vec<FrbMemoryLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    use crate::modules::memory_game::domain::MemoryGameRepository;

    // Unified sync: one relay round-trip populates all game caches.
    if let Some(state) = global_app_state() {
        crate::utils::leaderboard_relay::sync_all_leaderboards(state, false).await;
    }

    // Return merged leaderboard (same logic as memory_game_leaderboard)
    let peer_scores = game_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbMemoryLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbMemoryLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score PER DIFFICULTY so the user appears in
    // every difficulty filter they've played, not just their overall best.
    {
        use sea_orm::{ConnectionTrait, Statement};
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT difficulty, MAX(normalized_score) as best, played_at FROM memory_game_scores GROUP BY difficulty".to_owned(),
            ))
            .await
            .unwrap_or_default();

        for row in rows {
            if let (Ok(difficulty), Ok(best_score), Ok(played_at)) = (
                row.try_get::<String>("", "difficulty"),
                row.try_get::<f64>("", "best"),
                row.try_get::<String>("", "played_at"),
            ) {
                entries.push(FrbMemoryLeaderboardEntry {
                    peer_id: 0,
                    library_name: library_name.clone(),
                    best_score,
                    difficulty,
                    played_at,
                    is_self: true,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

// ============ Sliding Puzzle (FFI) ============

/// A generated puzzle board (FFI-safe)
pub struct FrbPuzzleBoard {
    pub book_id: String,
    pub title: String,
    pub cover_url: String,
    pub grid_size: u8,
    pub tiles: Vec<u8>,
    pub empty_index: u32,
    pub par_moves: u32,
}

/// A saved sliding puzzle score (FFI-safe)
pub struct FrbPuzzleScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub grid_size: i32,
    pub elapsed_seconds: f64,
    pub move_count: i32,
    pub par_moves: i32,
    pub normalized_score: f64,
    pub played_at: String,
    /// Achievements unlocked after this game (empty if none)
    pub new_achievements: Vec<String>,
}

/// Get available puzzle difficulty levels based on books with covers
pub async fn puzzle_available_difficulties() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let difficulties = crate::modules::sliding_puzzle::service::available_difficulties(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(difficulties
        .iter()
        .map(|d| d.as_str().to_string())
        .collect())
}

/// Set up a new sliding puzzle with the given difficulty
pub async fn puzzle_setup(difficulty: String) -> Result<FrbPuzzleBoard, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let diff = crate::modules::sliding_puzzle::service::PuzzleDifficulty::parse(&difficulty)
        .map_err(|e| e.to_string())?;
    let board = crate::modules::sliding_puzzle::service::setup_game(&repo, diff)
        .await
        .map_err(|e| e.to_string())?;
    Ok(FrbPuzzleBoard {
        book_id: board.book_id,
        title: board.title,
        cover_url: board.cover_url,
        grid_size: board.grid_size,
        tiles: board.tiles,
        empty_index: board.empty_index as u32,
        par_moves: board.par_moves,
    })
}

/// Submit a completed sliding puzzle and get the score back
pub async fn puzzle_finish(
    difficulty: String,
    grid_size: u8,
    elapsed_seconds: f64,
    move_count: u32,
    par_moves: u32,
) -> Result<FrbPuzzleScore, String> {
    let db = db().ok_or("Database not initialized")?;
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let result = crate::modules::sliding_puzzle::domain::PuzzleResult {
        difficulty,
        grid_size,
        elapsed_seconds,
        move_count,
        par_moves,
    };
    let score = crate::modules::sliding_puzzle::service::finish_game(&puzzle_repo, result)
        .await
        .map_err(|e| e.to_string())?;

    // Check achievements after game completion
    let new_achievements = {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        let game_repo =
            crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
        let hangman_repo =
            crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
        crate::services::gamification_service::check_and_unlock_achievements(
            &gamification_repo,
            &game_repo,
            Some(&puzzle_repo),
            Some(&hangman_repo),
        )
        .await
        .unwrap_or_default()
    };

    Ok(FrbPuzzleScore {
        id: score.id,
        difficulty: score.difficulty,
        grid_size: score.grid_size,
        elapsed_seconds: score.elapsed_seconds,
        move_count: score.move_count,
        par_moves: score.par_moves,
        normalized_score: score.normalized_score,
        played_at: score.played_at,
        new_achievements,
    })
}

/// Get top sliding puzzle scores
pub async fn puzzle_top_scores() -> Result<Vec<FrbPuzzleScore>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
    let scores = repo.get_top_scores(10).await.map_err(|e| e.to_string())?;
    Ok(scores
        .into_iter()
        .map(|s| FrbPuzzleScore {
            id: s.id,
            difficulty: s.difficulty,
            grid_size: s.grid_size,
            elapsed_seconds: s.elapsed_seconds,
            move_count: s.move_count,
            par_moves: s.par_moves,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
            new_achievements: vec![],
        })
        .collect())
}

/// A leaderboard entry for the sliding puzzle (FFI-safe)
pub struct FrbPuzzleLeaderboardEntry {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
    pub is_self: bool,
}

/// Get puzzle leaderboard (peer scores + local user's best)
pub async fn puzzle_game_leaderboard() -> Result<Vec<FrbPuzzleLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;

    // Peer scores
    let peer_scores = puzzle_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbPuzzleLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbPuzzleLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score PER DIFFICULTY
    {
        use sea_orm::{ConnectionTrait, Statement};
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT difficulty, MAX(normalized_score) as best, played_at FROM sliding_puzzle_scores GROUP BY difficulty".to_owned(),
            ))
            .await
            .unwrap_or_default();

        for row in rows {
            if let (Ok(difficulty), Ok(best_score), Ok(played_at)) = (
                row.try_get::<String>("", "difficulty"),
                row.try_get::<f64>("", "best"),
                row.try_get::<String>("", "played_at"),
            ) {
                entries.push(FrbPuzzleLeaderboardEntry {
                    peer_id: 0,
                    library_name: library_name.clone(),
                    best_score,
                    difficulty,
                    played_at,
                    is_self: true,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

/// Refresh the network puzzle leaderboard by syncing with all accepted peers.
/// Fetches each peer's /api/game/puzzle/public-best, upserts into peer_puzzle_scores,
/// then returns the merged leaderboard.
pub async fn puzzle_game_refresh_leaderboard() -> Result<Vec<FrbPuzzleLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;

    // Always sync peer scores on refresh -- the user explicitly requested it.
    if let Some(state) = global_app_state() {
        crate::utils::leaderboard_relay::sync_all_leaderboards(state, false).await;
    }

    // Return merged leaderboard (same logic as puzzle_game_leaderboard)
    let peer_scores = puzzle_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbPuzzleLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbPuzzleLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score PER DIFFICULTY
    {
        use sea_orm::{ConnectionTrait, Statement};
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT difficulty, MAX(normalized_score) as best, played_at FROM sliding_puzzle_scores GROUP BY difficulty".to_owned(),
            ))
            .await
            .unwrap_or_default();

        for row in rows {
            if let (Ok(difficulty), Ok(best_score), Ok(played_at)) = (
                row.try_get::<String>("", "difficulty"),
                row.try_get::<f64>("", "best"),
                row.try_get::<String>("", "played_at"),
            ) {
                entries.push(FrbPuzzleLeaderboardEntry {
                    peer_id: 0,
                    library_name: library_name.clone(),
                    best_score,
                    difficulty,
                    played_at,
                    is_self: true,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

// ─── Hangman (FFI direct) ───────────────────────────────────────────────────

/// A character in the hangman display (FFI-safe)
pub struct FrbHangmanChar {
    pub character: String,
    pub base_char: String,
    pub revealed: bool,
    pub is_guessable: bool,
}

/// Game setup returned to Flutter (FFI-safe)
pub struct FrbHangmanSetup {
    pub book_id: String,
    pub title: String,
    pub display: Vec<FrbHangmanChar>,
    pub author: String,
    pub cover_url: Option<String>,
    pub max_errors: u8,
    pub hints_available: u8,
    pub difficulty: String,
}

/// A saved hangman score (FFI-safe)
pub struct FrbHangmanScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub hints_used: i32,
    pub won: bool,
    pub normalized_score: f64,
    pub played_at: String,
    /// Achievements unlocked after this game (empty if none)
    pub new_achievements: Vec<String>,
}

/// A hangman leaderboard entry (FFI-safe)
pub struct FrbHangmanLeaderboardEntry {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
    pub is_self: bool,
}

/// Get available hangman difficulty levels based on valid titles count
pub async fn hangman_available_difficulties() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    let difficulties = crate::modules::hangman::service::available_difficulties(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(difficulties
        .iter()
        .map(|d| d.as_str().to_string())
        .collect())
}

/// Set up a new hangman game with the given difficulty.
/// `exclude_book_ids` -- book IDs already played in the current session (avoids same series).
pub async fn hangman_setup(
    difficulty: String,
    exclude_book_ids: Vec<String>,
) -> Result<FrbHangmanSetup, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    let diff = crate::modules::hangman::service::HangmanDifficulty::parse(&difficulty)
        .map_err(|e| e.to_string())?;
    let setup = crate::modules::hangman::service::setup_game(&repo, diff, &exclude_book_ids)
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbHangmanSetup {
        book_id: setup.book_id,
        title: setup.title,
        display: setup
            .display
            .into_iter()
            .map(|c| FrbHangmanChar {
                character: c.character.to_string(),
                base_char: c.base_char.to_string(),
                revealed: c.revealed,
                is_guessable: c.is_guessable,
            })
            .collect(),
        author: setup.author,
        cover_url: setup.cover_url,
        max_errors: setup.max_errors,
        hints_available: setup.hints_available,
        difficulty: setup.difficulty,
    })
}

/// Submit a completed hangman game and get the score back
pub async fn hangman_finish(
    book_id: String,
    difficulty: String,
    elapsed_seconds: f64,
    errors: i32,
    hints_used: i32,
    won: bool,
) -> Result<FrbHangmanScore, String> {
    let db = db().ok_or("Database not initialized")?;
    let hangman_repo =
        crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    let result = crate::modules::hangman::domain::HangmanResult {
        book_id,
        difficulty,
        elapsed_seconds,
        errors,
        hints_used,
        won,
    };
    let score = crate::modules::hangman::service::finish_game(&hangman_repo, result)
        .await
        .map_err(|e| e.to_string())?;

    // Check achievements after game completion
    let new_achievements = {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        let game_repo =
            crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
        let puzzle_repo =
            crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
        crate::services::gamification_service::check_and_unlock_achievements(
            &gamification_repo,
            &game_repo,
            Some(&puzzle_repo),
            Some(&hangman_repo),
        )
        .await
        .unwrap_or_default()
    };

    Ok(FrbHangmanScore {
        id: score.id,
        difficulty: score.difficulty,
        elapsed_seconds: score.elapsed_seconds,
        errors: score.errors,
        hints_used: score.hints_used,
        won: score.won,
        normalized_score: score.normalized_score,
        played_at: score.played_at,
        new_achievements,
    })
}

/// Get top hangman scores
pub async fn hangman_top_scores() -> Result<Vec<FrbHangmanScore>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    use crate::modules::hangman::domain::HangmanRepository;
    let scores = repo.get_top_scores(10).await.map_err(|e| e.to_string())?;
    Ok(scores
        .into_iter()
        .map(|s| FrbHangmanScore {
            id: s.id,
            difficulty: s.difficulty,
            elapsed_seconds: s.elapsed_seconds,
            errors: s.errors,
            hints_used: s.hints_used,
            won: s.won,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
            new_achievements: vec![],
        })
        .collect())
}

/// Get hangman leaderboard (peer scores + local user's best)
pub async fn hangman_leaderboard() -> Result<Vec<FrbHangmanLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let hangman_repo =
        crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    use crate::modules::hangman::domain::HangmanRepository;

    let peer_scores = hangman_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbHangmanLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbHangmanLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score PER DIFFICULTY
    {
        use sea_orm::{ConnectionTrait, Statement};
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT difficulty, MAX(normalized_score) as best, played_at FROM hangman_scores GROUP BY difficulty".to_owned(),
            ))
            .await
            .unwrap_or_default();

        for row in rows {
            if let (Ok(difficulty), Ok(best_score), Ok(played_at)) = (
                row.try_get::<String>("", "difficulty"),
                row.try_get::<f64>("", "best"),
                row.try_get::<String>("", "played_at"),
            ) {
                entries.push(FrbHangmanLeaderboardEntry {
                    peer_id: 0,
                    library_name: library_name.clone(),
                    best_score,
                    difficulty,
                    played_at,
                    is_self: true,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

/// Refresh the hangman leaderboard by syncing with all accepted peers
pub async fn hangman_refresh_leaderboard() -> Result<Vec<FrbHangmanLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let hangman_repo =
        crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    use crate::modules::hangman::domain::HangmanRepository;

    // Always sync peer scores on refresh -- the user explicitly requested it.
    if let Some(state) = global_app_state() {
        crate::utils::leaderboard_relay::sync_all_leaderboards(state, false).await;
    }

    let peer_scores = hangman_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbHangmanLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbHangmanLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score PER DIFFICULTY
    {
        use sea_orm::{ConnectionTrait, Statement};
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT difficulty, MAX(normalized_score) as best, played_at FROM hangman_scores GROUP BY difficulty".to_owned(),
            ))
            .await
            .unwrap_or_default();

        for row in rows {
            if let (Ok(difficulty), Ok(best_score), Ok(played_at)) = (
                row.try_get::<String>("", "difficulty"),
                row.try_get::<f64>("", "best"),
                row.try_get::<String>("", "played_at"),
            ) {
                entries.push(FrbHangmanLeaderboardEntry {
                    peer_id: 0,
                    library_name: library_name.clone(),
                    best_score,
                    difficulty,
                    played_at,
                    is_self: true,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}
