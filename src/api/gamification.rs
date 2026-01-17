use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
};
use serde::Serialize;

use crate::models::{
    book, gamification_achievements, gamification_config, gamification_streaks, loan,
};

// Track thresholds configuration
// All tracks: 6 levels - Novice (25), Apprenti (50), Bronze (100), Argent (250), Or (500), Platine (1000)
const COLLECTOR_THRESHOLDS: [i32; 6] = [25, 50, 100, 250, 500, 1000];
const COLLECTOR_STEP: i32 = 250; // Books per prestige level after Platine

const READER_THRESHOLDS: [i32; 6] = [25, 50, 100, 250, 500, 1000];
const READER_STEP: i32 = 250; // Reads per prestige level

const LENDER_THRESHOLDS: [i32; 6] = [25, 50, 100, 250, 500, 1000];
const LENDER_STEP: i32 = 250; // Loans per prestige level

const CATALOGUER_THRESHOLDS: [i32; 6] = [25, 50, 100, 250, 500, 1000];
const CATALOGUER_STEP: i32 = 250; // Organized books per prestige level

#[derive(Serialize)]
pub struct TrackProgress {
    pub level: i32,          // 0=Curieux, 1=Initié, 2=Bibliophile, 3=Érudit
    pub progress: f32,       // 0.0 to 1.0 progress to next level
    pub current: i64,        // Current value
    pub next_threshold: i32, // Next level threshold
}

#[derive(Serialize)]
pub struct StreakInfo {
    pub current: i32,
    pub longest: i32,
}

#[derive(Serialize)]
pub struct GamificationConfigDto {
    pub achievements_style: String,
    pub reading_goal_yearly: i32,
    pub reading_goal_progress: i32,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
pub struct TracksStatus {
    pub collector: TrackProgress,
    pub reader: TrackProgress,
    pub lender: TrackProgress,
    pub cataloguer: TrackProgress,
}

fn calculate_track_progress(
    current: i64,
    thresholds: &[i32; 6],
    prestige_step: i32,
) -> TrackProgress {
    let current_val = current as i32;

    // Check for Prestige Levels (Level > 6)
    if current_val >= thresholds[5] {
        let excess = current_val - thresholds[5];
        let prestige_levels = excess / prestige_step;
        let level = 6 + prestige_levels;

        let current_step_progress = excess % prestige_step;
        let next_threshold = thresholds[5] + (prestige_levels + 1) * prestige_step;

        let progress = current_step_progress as f32 / prestige_step as f32;

        return TrackProgress {
            level,
            progress: progress.clamp(0.0, 1.0),
            current,
            next_threshold,
        };
    }

    // Standard levels (0-6)
    let (level, next_threshold) = if current_val >= thresholds[4] {
        (5, thresholds[5]) // Or, progressing to Platine
    } else if current_val >= thresholds[3] {
        (4, thresholds[4]) // Argent, progressing to Or
    } else if current_val >= thresholds[2] {
        (3, thresholds[3]) // Bronze, progressing to Argent
    } else if current_val >= thresholds[1] {
        (2, thresholds[2]) // Apprenti, progressing to Bronze
    } else if current_val >= thresholds[0] {
        (1, thresholds[1]) // Novice, progressing to Apprenti
    } else {
        (0, thresholds[0]) // Curieux, progressing to Novice
    };

    let prev_threshold = match level {
        0 => 0,
        1 => thresholds[0],
        2 => thresholds[1],
        3 => thresholds[2],
        4 => thresholds[3],
        5 => thresholds[4],
        _ => thresholds[5],
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

pub async fn get_user_status(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // For V3 single-user mode, use user_id = 1
    let user_id = 1;

    // 1. Count books (Collector Track)
    let books_count = book::Entity::find().count(&db).await.unwrap_or(0) as i64;

    // 2. Count books with reading_status = 'read' (Reader Track)
    let read_count = book::Entity::find()
        .filter(book::Column::ReadingStatus.eq("read"))
        .count(&db)
        .await
        .unwrap_or(0) as i64;

    // 3. Count loans (Lender Track)
    let loans_count = loan::Entity::find().count(&db).await.unwrap_or(0) as i64;

    // 4. Count books with custom shelf order (Cataloguer Track)
    let organized_count = book::Entity::find()
        .filter(book::Column::ShelfPosition.gt(0))
        .count(&db)
        .await
        .unwrap_or(0) as i64;

    // Calculate track progress
    let collector_progress =
        calculate_track_progress(books_count, &COLLECTOR_THRESHOLDS, COLLECTOR_STEP);
    let reader_progress = calculate_track_progress(read_count, &READER_THRESHOLDS, READER_STEP);
    let lender_progress = calculate_track_progress(loans_count, &LENDER_THRESHOLDS, LENDER_STEP);
    let cataloguer_progress =
        calculate_track_progress(organized_count, &CATALOGUER_THRESHOLDS, CATALOGUER_STEP);

    // Get streak info
    let streak = gamification_streaks::Entity::find()
        .filter(gamification_streaks::Column::UserId.eq(user_id))
        .one(&db)
        .await
        .ok()
        .flatten()
        .map(|s| StreakInfo {
            current: s.current_streak,
            longest: s.longest_streak,
        })
        .unwrap_or(StreakInfo {
            current: 0,
            longest: 0,
        });

    // Get recent achievements (last 5)
    let recent_achievements = gamification_achievements::Entity::find()
        .filter(gamification_achievements::Column::UserId.eq(user_id))
        .order_by_desc(gamification_achievements::Column::UnlockedAt)
        .all(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .take(5)
        .map(|a| a.achievement_id)
        .collect::<Vec<_>>();

    // Get config
    let config = gamification_config::Entity::find()
        .filter(gamification_config::Column::UserId.eq(user_id))
        .one(&db)
        .await
        .ok()
        .flatten();

    let config_dto = config
        .map(|c| GamificationConfigDto {
            achievements_style: c.achievements_style,
            reading_goal_yearly: c.reading_goal_yearly,
            reading_goal_progress: read_count as i32,
        })
        .unwrap_or(GamificationConfigDto {
            achievements_style: "minimal".to_string(),
            reading_goal_yearly: 12,
            reading_goal_progress: read_count as i32,
        });

    // Legacy level calculation (for backward compatibility with Flutter)
    let max_level = collector_progress
        .level
        .max(reader_progress.level)
        .max(lender_progress.level);
    let legacy_level = match max_level {
        3 => "Pro",
        2 | 1 => "BiblioGenius",
        _ => "Member",
    };

    // Legacy progress (average of all tracks)
    let legacy_progress =
        (collector_progress.progress + reader_progress.progress + lender_progress.progress) / 3.0;

    let status = UserStatusV2 {
        tracks: TracksStatus {
            collector: collector_progress,
            reader: reader_progress,
            lender: lender_progress,
            cataloguer: cataloguer_progress,
        },
        streak,
        recent_achievements,
        config: config_dto,
        // Legacy fields
        level: legacy_level.to_string(),
        loans_count: loans_count as u64,
        edits_count: books_count as u64, // Use books_count as proxy for edits
        next_level_progress: legacy_progress,
        badge_url: format!("assets/badges/{}.png", legacy_level.to_lowercase()),
    };

    (StatusCode::OK, Json(status)).into_response()
}
