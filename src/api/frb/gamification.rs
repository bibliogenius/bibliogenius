// Gamification: reading tracks, streaks, leaderboard.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ─── Gamification (FFI direct) ──────────────────────────────────────────────

/// Track progress (FFI-safe)
pub struct FrbTrackProgress {
    pub level: i32,
    pub progress: f32,
    pub current: i64,
    pub next_threshold: i32,
}

/// Streak info (FFI-safe)
pub struct FrbStreakInfo {
    pub current: i32,
    pub longest: i32,
}

/// Gamification config (FFI-safe)
pub struct FrbGamificationConfig {
    pub achievements_style: String,
    pub reading_goal_yearly: i32,
    pub reading_goal_progress: i32,
    pub total_books_read: i32,
}

/// Full gamification status (FFI-safe)
pub struct FrbGamificationStatus {
    pub collector: FrbTrackProgress,
    pub reader: FrbTrackProgress,
    pub lender: FrbTrackProgress,
    pub cataloguer: FrbTrackProgress,
    pub streak: FrbStreakInfo,
    pub recent_achievements: Vec<String>,
    pub config: FrbGamificationConfig,
    // Legacy fields
    pub level: String,
    pub loans_count: i64,
    pub edits_count: i64,
    pub next_level_progress: f32,
    pub badge_url: String,
}

/// Leaderboard entry (FFI-safe)
pub struct FrbLeaderboardEntry {
    pub library_name: String,
    pub level: i32,
    pub current: i64,
    pub is_self: bool,
    pub peer_id: Option<i32>,
}

/// Full leaderboard response (FFI-safe)
pub struct FrbLeaderboardResponse {
    pub collector: Vec<FrbLeaderboardEntry>,
    pub reader: Vec<FrbLeaderboardEntry>,
    pub lender: Vec<FrbLeaderboardEntry>,
    pub cataloguer: Vec<FrbLeaderboardEntry>,
    pub last_refreshed: Option<String>,
}

fn track_to_frb(t: &crate::services::gamification_service::TrackProgress) -> FrbTrackProgress {
    FrbTrackProgress {
        level: t.level,
        progress: t.progress,
        current: t.current,
        next_threshold: t.next_threshold,
    }
}

fn entries_to_frb(
    entries: &[crate::services::gamification_service::LeaderboardEntry],
) -> Vec<FrbLeaderboardEntry> {
    entries
        .iter()
        .map(|e| FrbLeaderboardEntry {
            library_name: e.library_name.clone(),
            level: e.level,
            current: e.current,
            is_self: e.is_self,
            peer_id: e.peer_id,
        })
        .collect()
}

/// Get full gamification status via FFI (replaces HTTP getUserStatus)
pub async fn gamification_get_status() -> Result<FrbGamificationStatus, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let status = crate::services::gamification_service::get_user_status(&repo)
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbGamificationStatus {
        collector: track_to_frb(&status.tracks.collector),
        reader: track_to_frb(&status.tracks.reader),
        lender: track_to_frb(&status.tracks.lender),
        cataloguer: track_to_frb(&status.tracks.cataloguer),
        streak: FrbStreakInfo {
            current: status.streak.current,
            longest: status.streak.longest,
        },
        recent_achievements: status.recent_achievements,
        config: FrbGamificationConfig {
            achievements_style: status.config.achievements_style,
            reading_goal_yearly: status.config.reading_goal_yearly,
            reading_goal_progress: status.config.reading_goal_progress,
            total_books_read: status.config.total_books_read,
        },
        level: status.level,
        loans_count: status.loans_count as i64,
        edits_count: status.edits_count as i64,
        next_level_progress: status.next_level_progress,
        badge_url: status.badge_url,
    })
}

/// Get leaderboard via FFI (replaces HTTP getLeaderboard)
pub async fn gamification_get_leaderboard() -> Result<FrbLeaderboardResponse, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let lb = crate::services::gamification_service::build_leaderboard(&repo)
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbLeaderboardResponse {
        collector: entries_to_frb(&lb.collector),
        reader: entries_to_frb(&lb.reader),
        lender: entries_to_frb(&lb.lender),
        cataloguer: entries_to_frb(&lb.cataloguer),
        last_refreshed: lb.last_refreshed,
    })
}

/// Refresh leaderboard (returns current state) via FFI.
/// Peer sync happens via the HTTP endpoint - this just returns current data.
pub async fn gamification_refresh_leaderboard() -> Result<FrbLeaderboardResponse, String> {
    gamification_get_leaderboard().await
}

/// Update gamification config via FFI
pub async fn gamification_update_config(
    reading_goal_yearly: Option<i32>,
    achievements_style: Option<String>,
) -> Result<(), String> {
    use crate::domain::GamificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let user_id = repo.get_user_id().await.map_err(|e| e.to_string())?;
    let update = crate::domain::GamificationConfigUpdate {
        reading_goal_yearly,
        achievements_style,
    };
    repo.update_config(user_id, update)
        .await
        .map_err(|e| e.to_string())
}

/// Check and unlock eligible achievements via FFI
pub async fn gamification_check_achievements() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
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
    .map_err(|e| e.to_string())
}

/// Update daily streak via FFI
pub async fn gamification_update_streak() -> Result<FrbStreakInfo, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let streak = crate::services::gamification_service::update_streak(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(FrbStreakInfo {
        current: streak.current,
        longest: streak.longest,
    })
}
