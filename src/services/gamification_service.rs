//! Gamification service — business logic for tracks, streaks, achievements, and leaderboard.
//!
//! All DB access goes through GamificationRepository trait.
//! This service is called from both Axum handlers and FFI bindings.

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::domain::{DomainError, GamificationRepository};
use crate::modules::hangman::domain::HangmanRepository;
use crate::modules::memory_game::domain::MemoryGameRepository;
use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;

// ─── Track thresholds ───────────────────────────────────────────────────────

/// All tracks share the same thresholds:
/// Novice (25), Apprenti (50), Bronze (100), Argent (250), Or (500), Platine (1000)
const THRESHOLDS: [i32; 6] = [25, 50, 100, 250, 500, 1000];
const PRESTIGE_STEP: i32 = 250;

// ─── Public types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TrackProgress {
    pub level: i32,
    pub progress: f32,
    pub current: i64,
    pub next_threshold: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct TracksStatus {
    pub collector: TrackProgress,
    pub reader: TrackProgress,
    pub lender: TrackProgress,
    pub cataloguer: TrackProgress,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreakInfo {
    pub current: i32,
    pub longest: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GamificationConfigDto {
    pub achievements_style: String,
    pub reading_goal_yearly: i32,
    pub reading_goal_progress: i32,
    pub total_books_read: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserStatusV2 {
    pub tracks: TracksStatus,
    pub streak: StreakInfo,
    pub recent_achievements: Vec<String>,
    pub config: GamificationConfigDto,
    // Legacy fields for backward compatibility
    pub level: String,
    pub loans_count: u64,
    pub edits_count: u64,
    pub next_level_progress: f32,
    pub badge_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicTrackStats {
    pub level: i32,
    pub current: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicGamificationStats {
    pub library_name: String,
    pub collector: PublicTrackStats,
    pub reader: PublicTrackStats,
    pub lender: PublicTrackStats,
    pub cataloguer: PublicTrackStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardEntry {
    pub library_name: String,
    pub level: i32,
    pub current: i64,
    pub is_self: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardResponse {
    pub collector: Vec<LeaderboardEntry>,
    pub reader: Vec<LeaderboardEntry>,
    pub lender: Vec<LeaderboardEntry>,
    pub cataloguer: Vec<LeaderboardEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refreshed: Option<String>,
}

// ─── Pure functions ─────────────────────────────────────────────────────────

/// Calculate track progress from a raw count.
///
/// Levels 0-6 follow fixed thresholds [25, 50, 100, 250, 500, 1000].
/// Beyond 1000, prestige levels are earned every 250 items.
pub fn calculate_track_progress(current: i64) -> TrackProgress {
    let current_val = current as i32;

    // Prestige levels (beyond Platine)
    if current_val >= THRESHOLDS[5] {
        let excess = current_val - THRESHOLDS[5];
        let prestige_levels = excess / PRESTIGE_STEP;
        let level = 6 + prestige_levels;
        let next_threshold = THRESHOLDS[5] + (prestige_levels + 1) * PRESTIGE_STEP;
        let progress_in_step = excess % PRESTIGE_STEP;
        let progress = progress_in_step as f32 / PRESTIGE_STEP as f32;

        return TrackProgress {
            level,
            progress: progress.clamp(0.0, 1.0),
            current,
            next_threshold,
        };
    }

    // Standard levels (0-5)
    let (level, next_threshold) = if current_val >= THRESHOLDS[4] {
        (5, THRESHOLDS[5])
    } else if current_val >= THRESHOLDS[3] {
        (4, THRESHOLDS[4])
    } else if current_val >= THRESHOLDS[2] {
        (3, THRESHOLDS[3])
    } else if current_val >= THRESHOLDS[1] {
        (2, THRESHOLDS[2])
    } else if current_val >= THRESHOLDS[0] {
        (1, THRESHOLDS[1])
    } else {
        (0, THRESHOLDS[0])
    };

    let prev_threshold = match level {
        0 => 0,
        1 => THRESHOLDS[0],
        2 => THRESHOLDS[1],
        3 => THRESHOLDS[2],
        4 => THRESHOLDS[3],
        5 => THRESHOLDS[4],
        _ => THRESHOLDS[5],
    };

    let range = (next_threshold - prev_threshold) as f32;
    let progress_in_range = (current_val - prev_threshold) as f32;
    let progress = (progress_in_range / range).clamp(0.0, 1.0);

    TrackProgress {
        level,
        progress,
        current,
        next_threshold,
    }
}

// ─── Service functions ──────────────────────────────────────────────────────

/// Get the full user gamification status.
pub async fn get_user_status(
    repo: &dyn GamificationRepository,
) -> Result<UserStatusV2, DomainError> {
    let user_id = repo.get_user_id().await?;
    let current_year = Utc::now().format("%Y").to_string();

    // Parallel group 1: COUNT queries
    let (books_count, read_count, yearly_read_count, loans_count, organized_count) = tokio::join!(
        repo.count_books(),
        repo.count_books_read(),
        repo.count_books_read_in_year(&current_year),
        repo.count_loans(),
        repo.count_catalogued_books(),
    );

    let books_count = books_count?;
    let read_count = read_count?;
    let yearly_read_count = yearly_read_count?;
    let loans_count = loans_count?;
    let organized_count = organized_count?;

    // Calculate track progress
    let collector = calculate_track_progress(books_count);
    let reader = calculate_track_progress(read_count);
    let lender = calculate_track_progress(loans_count);
    let cataloguer = calculate_track_progress(organized_count);

    // Parallel group 2: Streak, achievements, config
    let (streak_result, achievements_result, config_result) = tokio::join!(
        repo.get_streak(user_id),
        repo.get_recent_achievements(user_id, 50),
        repo.get_config(user_id),
    );

    let streak = streak_result?
        .map(|(current, longest, _)| StreakInfo { current, longest })
        .unwrap_or(StreakInfo {
            current: 0,
            longest: 0,
        });

    let recent_achievements = achievements_result?;

    let config = config_result?;

    let config_dto = config
        .map(|c| GamificationConfigDto {
            achievements_style: c.achievements_style,
            reading_goal_yearly: c.reading_goal_yearly,
            reading_goal_progress: yearly_read_count as i32,
            total_books_read: read_count as i32,
        })
        .unwrap_or(GamificationConfigDto {
            achievements_style: "minimal".to_string(),
            reading_goal_yearly: 12,
            reading_goal_progress: yearly_read_count as i32,
            total_books_read: read_count as i32,
        });

    // Legacy level calculation
    let max_level = collector.level.max(reader.level).max(lender.level);
    let legacy_level = match max_level {
        3.. => "Pro",
        1..=2 => "BiblioGenius",
        _ => "Member",
    };

    let legacy_progress = (collector.progress + reader.progress + lender.progress) / 3.0;

    Ok(UserStatusV2 {
        tracks: TracksStatus {
            collector,
            reader,
            lender,
            cataloguer,
        },
        streak,
        recent_achievements,
        config: config_dto,
        level: legacy_level.to_string(),
        loans_count: loans_count as u64,
        edits_count: books_count as u64,
        next_level_progress: legacy_progress,
        badge_url: format!("assets/badges/{}.png", legacy_level.to_lowercase()),
    })
}

/// Get public gamification stats (returns None if sharing is disabled).
pub async fn get_public_stats(
    repo: &dyn GamificationRepository,
) -> Result<Option<PublicGamificationStats>, DomainError> {
    // Check if sharing is enabled
    let network_enabled = repo.is_module_enabled("network_gamification").await?;
    let share_enabled = repo.is_module_enabled("share_gamification_stats").await?;

    if !network_enabled || !share_enabled {
        return Ok(None);
    }

    let library_name = repo.get_library_name().await?;

    let (books, reads, loans, organized) = tokio::join!(
        repo.count_books(),
        repo.count_books_read(),
        repo.count_loans(),
        repo.count_catalogued_books(),
    );

    let collector = calculate_track_progress(books?);
    let reader = calculate_track_progress(reads?);
    let lender = calculate_track_progress(loans?);
    let cataloguer = calculate_track_progress(organized?);

    Ok(Some(PublicGamificationStats {
        library_name,
        collector: PublicTrackStats {
            level: collector.level,
            current: collector.current,
        },
        reader: PublicTrackStats {
            level: reader.level,
            current: reader.current,
        },
        lender: PublicTrackStats {
            level: lender.level,
            current: lender.current,
        },
        cataloguer: PublicTrackStats {
            level: cataloguer.level,
            current: cataloguer.current,
        },
    }))
}

/// Build the combined leaderboard (local + peers).
/// Returns Err if network_gamification is disabled.
pub async fn build_leaderboard(
    repo: &dyn GamificationRepository,
) -> Result<LeaderboardResponse, DomainError> {
    let network_enabled = repo.is_module_enabled("network_gamification").await?;
    if !network_enabled {
        return Err(DomainError::Validation(
            "Network gamification is disabled".to_string(),
        ));
    }

    let library_name = repo.get_library_name().await?;

    // Compute local stats
    let (books, reads, loans, organized) = tokio::join!(
        repo.count_books(),
        repo.count_books_read(),
        repo.count_loans(),
        repo.count_catalogued_books(),
    );

    let local_collector = calculate_track_progress(books?);
    let local_reader = calculate_track_progress(reads?);
    let local_lender = calculate_track_progress(loans?);
    let local_cataloguer = calculate_track_progress(organized?);

    // Build entries starting with local
    let mut collector_entries = vec![LeaderboardEntry {
        library_name: library_name.clone(),
        level: local_collector.level,
        current: local_collector.current,
        is_self: true,
        peer_id: None,
    }];
    let mut reader_entries = vec![LeaderboardEntry {
        library_name: library_name.clone(),
        level: local_reader.level,
        current: local_reader.current,
        is_self: true,
        peer_id: None,
    }];
    let mut lender_entries = vec![LeaderboardEntry {
        library_name: library_name.clone(),
        level: local_lender.level,
        current: local_lender.current,
        is_self: true,
        peer_id: None,
    }];
    let mut cataloguer_entries = vec![LeaderboardEntry {
        library_name: library_name.clone(),
        level: local_cataloguer.level,
        current: local_cataloguer.current,
        is_self: true,
        peer_id: None,
    }];

    // Add peer stats
    let peer_stats = repo.get_peer_stats().await?;

    let last_refreshed: Option<String> = peer_stats
        .iter()
        .map(|s| s.synced_at.as_str())
        .min()
        .map(|s| s.to_string());

    for stat in peer_stats {
        collector_entries.push(LeaderboardEntry {
            library_name: stat.library_name.clone(),
            level: stat.collector_level,
            current: stat.collector_current as i64,
            is_self: false,
            peer_id: Some(stat.peer_id),
        });
        reader_entries.push(LeaderboardEntry {
            library_name: stat.library_name.clone(),
            level: stat.reader_level,
            current: stat.reader_current as i64,
            is_self: false,
            peer_id: Some(stat.peer_id),
        });
        lender_entries.push(LeaderboardEntry {
            library_name: stat.library_name.clone(),
            level: stat.lender_level,
            current: stat.lender_current as i64,
            is_self: false,
            peer_id: Some(stat.peer_id),
        });
        cataloguer_entries.push(LeaderboardEntry {
            library_name: stat.library_name.clone(),
            level: stat.cataloguer_level,
            current: stat.cataloguer_current as i64,
            is_self: false,
            peer_id: Some(stat.peer_id),
        });
    }

    // Sort: by level desc, then current desc
    let sort_fn = |a: &LeaderboardEntry, b: &LeaderboardEntry| match b.level.cmp(&a.level) {
        std::cmp::Ordering::Equal => b.current.cmp(&a.current),
        ord => ord,
    };

    collector_entries.sort_by(sort_fn);
    reader_entries.sort_by(sort_fn);
    lender_entries.sort_by(sort_fn);
    cataloguer_entries.sort_by(sort_fn);

    Ok(LeaderboardResponse {
        collector: collector_entries,
        reader: reader_entries,
        lender: lender_entries,
        cataloguer: cataloguer_entries,
        last_refreshed,
    })
}

/// Update the daily streak.
///
/// Returns the updated streak info.
pub async fn update_streak(repo: &dyn GamificationRepository) -> Result<StreakInfo, DomainError> {
    let user_id = repo.get_user_id().await?;
    let now = Utc::now();
    let today = now.format("%Y-%m-%d").to_string();
    let yesterday = (now - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    let (current_streak, longest_streak) = match repo.get_streak(user_id).await? {
        Some((current, longest, last_date)) => {
            if last_date.as_deref() == Some(today.as_str()) {
                // Already logged today
                return Ok(StreakInfo {
                    current: current.max(1),
                    longest: longest.max(current.max(1)),
                });
            }

            let new_current = if last_date.as_deref() == Some(yesterday.as_str()) {
                current + 1 // Continue streak
            } else {
                1 // Streak broken
            };
            let new_longest = longest.max(new_current);
            (new_current, new_longest)
        }
        None => (1, 1), // First ever
    };

    repo.update_streak(user_id, current_streak, longest_streak, &today)
        .await?;

    Ok(StreakInfo {
        current: current_streak,
        longest: longest_streak,
    })
}

/// Check and unlock eligible achievements. Returns list of newly unlocked achievement IDs.
pub async fn check_and_unlock_achievements(
    repo: &dyn GamificationRepository,
    game_repo: &dyn MemoryGameRepository,
    puzzle_repo: Option<&dyn SlidingPuzzleRepository>,
    hangman_repo: Option<&dyn HangmanRepository>,
) -> Result<Vec<String>, DomainError> {
    let user_id = repo.get_user_id().await?;
    let mut newly_unlocked = Vec::new();

    // Gather stats
    let (books, reads, loans) = tokio::join!(
        repo.count_books(),
        repo.count_books_read(),
        repo.count_loans(),
    );

    let books = books?;
    let reads = reads?;
    let loans = loans?;

    // Streak
    let streak = repo
        .get_streak(user_id)
        .await?
        .map(|(c, _, _)| c)
        .unwrap_or(0);

    // Memory game stats
    let memory_scores = game_repo.get_top_scores(100).await.unwrap_or_default();

    // Sliding puzzle stats
    let puzzle_scores = match puzzle_repo {
        Some(pr) => pr.get_top_scores(100).await.unwrap_or_default(),
        None => vec![],
    };

    // Define achievement checks
    let mut checks: Vec<(&str, bool)> = vec![
        ("first_book", books >= 1),
        ("collector_10", books >= 10),
        ("collector_100", books >= 100),
        ("first_read", reads >= 1),
        ("reader_10", reads >= 10),
        ("first_loan", loans >= 1),
        ("streak_7", streak >= 7),
        ("streak_30", streak >= 30),
        ("memory_first_game", !memory_scores.is_empty()),
        (
            "memory_perfect",
            memory_scores.iter().any(|s| s.errors == 0),
        ),
        (
            "memory_master",
            memory_scores.iter().any(|s| s.difficulty == "master"),
        ),
    ];

    // Sliding puzzle achievements
    checks.push(("puzzle_first_game", !puzzle_scores.is_empty()));
    checks.push((
        "puzzle_perfect",
        puzzle_scores.iter().any(|s| s.move_count <= s.par_moves),
    ));
    checks.push((
        "puzzle_master",
        puzzle_scores.iter().any(|s| s.grid_size == 5),
    ));

    // Hangman achievements
    let hangman_scores = match hangman_repo {
        Some(hr) => hr.get_top_scores(100).await.unwrap_or_default(),
        None => vec![],
    };
    checks.push(("hangman_first_game", !hangman_scores.is_empty()));
    checks.push((
        "hangman_perfect",
        hangman_scores
            .iter()
            .any(|s| s.won && s.errors == 0 && s.hints_used == 0),
    ));
    checks.push((
        "hangman_master",
        hangman_scores
            .iter()
            .any(|s| s.won && s.difficulty == "hard"),
    ));

    for (achievement_id, eligible) in checks {
        if eligible && repo.unlock_achievement(user_id, achievement_id).await? {
            newly_unlocked.push(achievement_id.to_string());
        }
    }

    Ok(newly_unlocked)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_track_progress_zero() {
        let tp = calculate_track_progress(0);
        assert_eq!(tp.level, 0);
        assert_eq!(tp.next_threshold, 25);
        assert_eq!(tp.progress, 0.0);
    }

    #[test]
    fn test_calculate_track_progress_novice() {
        let tp = calculate_track_progress(25);
        assert_eq!(tp.level, 1);
        assert_eq!(tp.next_threshold, 50);
        assert_eq!(tp.progress, 0.0);
    }

    #[test]
    fn test_calculate_track_progress_midway() {
        // 75 is between Bronze (100) threshold start (50) and next (100)
        // Level 2 (Apprenti), progress = (75-50)/(100-50) = 0.5
        let tp = calculate_track_progress(75);
        assert_eq!(tp.level, 2);
        assert_eq!(tp.next_threshold, 100);
        assert!((tp.progress - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_calculate_track_progress_platine() {
        let tp = calculate_track_progress(1000);
        assert_eq!(tp.level, 6);
        assert_eq!(tp.next_threshold, 1250);
        assert_eq!(tp.progress, 0.0);
    }

    #[test]
    fn test_calculate_track_progress_prestige() {
        // 1500 = 1000 + 500 = 2 prestige levels
        let tp = calculate_track_progress(1500);
        assert_eq!(tp.level, 8); // 6 + 2
        assert_eq!(tp.next_threshold, 1750);
        assert_eq!(tp.progress, 0.0);
    }

    #[test]
    fn test_calculate_track_progress_prestige_mid() {
        // 1125 = 1000 + 125 = half of first prestige step
        let tp = calculate_track_progress(1125);
        assert_eq!(tp.level, 6);
        assert_eq!(tp.next_threshold, 1250);
        assert!((tp.progress - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_calculate_track_progress_max_standard() {
        // Level 5 (Or), progress toward Platine
        let tp = calculate_track_progress(750);
        assert_eq!(tp.level, 5);
        assert_eq!(tp.next_threshold, 1000);
        assert!((tp.progress - 0.5).abs() < 0.01);
    }
}
