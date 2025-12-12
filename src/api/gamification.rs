use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
};
use serde::Serialize;

use crate::models::{
    book, gamification_achievements, gamification_config, gamification_streaks, loan,
};

// Track thresholds configuration
const COLLECTOR_THRESHOLDS: [i32; 3] = [10, 50, 200]; // Bronze, Silver, Gold
const READER_THRESHOLDS: [i32; 3] = [5, 20, 100];
const LENDER_THRESHOLDS: [i32; 3] = [5, 20, 50];

#[derive(Serialize)]
pub struct TrackProgress {
    pub level: i32,          // 0=None, 1=Bronze, 2=Silver, 3=Gold
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
}

fn calculate_track_progress(current: i64, thresholds: &[i32; 3]) -> TrackProgress {
    let (level, next_threshold) = if current >= thresholds[2] as i64 {
        (3, thresholds[2]) // Gold (max level)
    } else if current >= thresholds[1] as i64 {
        (2, thresholds[2]) // Silver, progressing to Gold
    } else if current >= thresholds[0] as i64 {
        (1, thresholds[1]) // Bronze, progressing to Silver
    } else {
        (0, thresholds[0]) // None, progressing to Bronze
    };

    let prev_threshold = match level {
        0 => 0,
        1 => thresholds[0],
        2 => thresholds[1],
        _ => thresholds[2],
    };

    let progress = if level == 3 {
        1.0 // Max level reached
    } else {
        let range = (next_threshold - prev_threshold) as f32;
        let progress_in_range = (current as i32 - prev_threshold) as f32;
        (progress_in_range / range).clamp(0.0, 1.0)
    };

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

    // Calculate track progress
    let collector_progress = calculate_track_progress(books_count, &COLLECTOR_THRESHOLDS);
    let reader_progress = calculate_track_progress(read_count, &READER_THRESHOLDS);
    let lender_progress = calculate_track_progress(loans_count, &LENDER_THRESHOLDS);

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
