use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::models::{
    book, book_tags, contact, gamification_achievements, gamification_config, gamification_streaks,
    loan,
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
    pub reading_goal_progress: i32, // Books read THIS YEAR (based on finished_reading_at)
    pub total_books_read: i32,      // Total books with reading_status='read' (all time)
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

/// Count distinct books assigned to at least one shelf (tag).
async fn count_catalogued_books(db: &DatabaseConnection) -> i64 {
    book_tags::Entity::find()
        .select_only()
        .column_as(
            sea_orm::sea_query::Expr::cust("COUNT(DISTINCT book_id)"),
            "count",
        )
        .into_tuple::<i64>()
        .one(db)
        .await
        .unwrap_or(None)
        .unwrap_or(0)
}

pub async fn get_user_status(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // For V3 single-user mode, use user_id = 1
    let user_id = 1;
    let current_year = Utc::now().format("%Y").to_string();

    // Parallel group 1: All independent COUNT queries
    let (books_count, read_count, yearly_read_count, loans_count, organized_count) = tokio::join!(
        async { book::Entity::find().count(&db).await.unwrap_or(0) as i64 },
        async {
            book::Entity::find()
                .filter(book::Column::ReadingStatus.eq("read"))
                .count(&db)
                .await
                .unwrap_or(0) as i64
        },
        async {
            book::Entity::find()
                .filter(book::Column::FinishedReadingAt.like(format!("{}%", current_year)))
                .count(&db)
                .await
                .unwrap_or(0) as i64
        },
        async { loan::Entity::find().count(&db).await.unwrap_or(0) as i64 },
        count_catalogued_books(&db),
    );

    // Calculate track progress
    let collector_progress =
        calculate_track_progress(books_count, &COLLECTOR_THRESHOLDS, COLLECTOR_STEP);
    let reader_progress = calculate_track_progress(read_count, &READER_THRESHOLDS, READER_STEP);
    let lender_progress = calculate_track_progress(loans_count, &LENDER_THRESHOLDS, LENDER_STEP);
    let cataloguer_progress =
        calculate_track_progress(organized_count, &CATALOGUER_THRESHOLDS, CATALOGUER_STEP);

    // Parallel group 2: Streak, achievements, config (independent)
    let (streak_result, achievements_result, config_result) = tokio::join!(
        gamification_streaks::Entity::find()
            .filter(gamification_streaks::Column::UserId.eq(user_id))
            .one(&db),
        gamification_achievements::Entity::find()
            .filter(gamification_achievements::Column::UserId.eq(user_id))
            .order_by_desc(gamification_achievements::Column::UnlockedAt)
            .all(&db),
        gamification_config::Entity::find()
            .filter(gamification_config::Column::UserId.eq(user_id))
            .one(&db),
    );

    let streak = streak_result
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

    let recent_achievements = achievements_result
        .unwrap_or_default()
        .into_iter()
        .take(5)
        .map(|a| a.achievement_id)
        .collect::<Vec<_>>();

    let config = config_result.ok().flatten();

    let config_dto = config
        .map(|c| GamificationConfigDto {
            achievements_style: c.achievements_style,
            reading_goal_yearly: c.reading_goal_yearly,
            reading_goal_progress: yearly_read_count as i32, // Books finished THIS YEAR
            total_books_read: read_count as i32,             // All-time read count
        })
        .unwrap_or(GamificationConfigDto {
            achievements_style: "minimal".to_string(),
            reading_goal_yearly: 12,
            reading_goal_progress: yearly_read_count as i32,
            total_books_read: read_count as i32,
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

// --- Network Gamification (Leaderboard) ---

#[derive(Serialize, Deserialize)]
pub struct PublicTrackStats {
    pub level: i32,
    pub current: i64,
}

#[derive(Serialize, Deserialize)]
pub struct PublicGamificationStats {
    pub library_name: String,
    pub collector: PublicTrackStats,
    pub reader: PublicTrackStats,
    pub lender: PublicTrackStats,
    pub cataloguer: PublicTrackStats,
}

/// GET /api/gamification/public-stats
/// Returns public gamification stats if the user opted-in to sharing.
pub async fn get_public_stats(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::{installation_profile, library_config};

    // Check if network_gamification + share_gamification_stats are enabled
    let profile = match installation_profile::Entity::find_by_id(1).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Module not available"})),
            )
                .into_response();
        }
    };

    let enabled_modules: Vec<String> =
        serde_json::from_str(&profile.enabled_modules).unwrap_or_default();

    if !enabled_modules.contains(&"network_gamification".to_string())
        || !enabled_modules.contains(&"share_gamification_stats".to_string())
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Gamification stats sharing is disabled"})),
        )
            .into_response();
    }

    // Get library name
    let library_name = match library_config::Entity::find_by_id(1).one(&db).await {
        Ok(Some(c)) => c.name,
        _ => "Unknown".to_string(),
    };

    // Compute stats (same logic as get_user_status)
    let books_count = book::Entity::find().count(&db).await.unwrap_or(0) as i64;
    let read_count = book::Entity::find()
        .filter(book::Column::ReadingStatus.eq("read"))
        .count(&db)
        .await
        .unwrap_or(0) as i64;
    let loans_count = loan::Entity::find().count(&db).await.unwrap_or(0) as i64;
    let organized_count = count_catalogued_books(&db).await;

    let collector = calculate_track_progress(books_count, &COLLECTOR_THRESHOLDS, COLLECTOR_STEP);
    let reader = calculate_track_progress(read_count, &READER_THRESHOLDS, READER_STEP);
    let lender = calculate_track_progress(loans_count, &LENDER_THRESHOLDS, LENDER_STEP);
    let cataloguer =
        calculate_track_progress(organized_count, &CATALOGUER_THRESHOLDS, CATALOGUER_STEP);

    (
        StatusCode::OK,
        Json(PublicGamificationStats {
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
        }),
    )
        .into_response()
}

#[derive(Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub library_name: String,
    pub level: i32,
    pub current: i64,
    pub is_self: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<i32>,
}

#[derive(Serialize)]
pub struct LeaderboardResponse {
    pub collector: Vec<LeaderboardEntry>,
    pub reader: Vec<LeaderboardEntry>,
    pub lender: Vec<LeaderboardEntry>,
    pub cataloguer: Vec<LeaderboardEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refreshed: Option<String>,
}

/// POST /api/gamification/refresh-leaderboard
/// Syncs gamification stats from all connected peers, then returns the leaderboard.
pub async fn refresh_leaderboard(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::{installation_profile, peer};

    // Check if network_gamification is enabled
    let profile = match installation_profile::Entity::find_by_id(1).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Module not available"})),
            )
                .into_response();
        }
    };

    let enabled_modules: Vec<String> =
        serde_json::from_str(&profile.enabled_modules).unwrap_or_default();

    if !enabled_modules.contains(&"network_gamification".to_string()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Network gamification is disabled"})),
        )
            .into_response();
    }

    // Fetch all connected peers and sync their gamification stats.
    // If a peer is unreachable, preserve its cached data.
    let peers = peer::Entity::find()
        .filter(peer::Column::ConnectionStatus.eq("accepted"))
        .all(&db)
        .await
        .unwrap_or_default();

    let client = crate::api::peer::get_safe_client();

    for p in &peers {
        let config_url = format!("{}/api/config", p.url);
        match client.get(&config_url).send().await {
            Ok(res) if res.status().is_success() => {
                let config = match res.json::<crate::api::setup::ConfigResponse>().await {
                    Ok(c) => c,
                    Err(_) => {
                        // Parse error — skip, preserve cached data
                        continue;
                    }
                };

                // Update peer name and corresponding contact if it changed
                if config.library_name != p.name
                    && let Ok(Some(peer_model)) = peer::Entity::find_by_id(p.id).one(&db).await
                {
                    let old_name = peer_model.name.clone();
                    let mut active: peer::ActiveModel = peer_model.into();
                    active.name = Set(config.library_name.clone());
                    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                    let _ = active.update(&db).await;

                    // Also update the linked contact (matched by old name + type "Library")
                    if let Ok(Some(contact_model)) = contact::Entity::find()
                        .filter(contact::Column::Name.eq(&old_name))
                        .filter(contact::Column::Type.eq("Library"))
                        .one(&db)
                        .await
                    {
                        let mut contact_active: contact::ActiveModel = contact_model.into();
                        contact_active.name = Set(config.library_name.clone());
                        contact_active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                        let _ = contact_active.update(&db).await;
                    }
                }

                crate::api::peer::sync_peer_gamification_stats(
                    &db,
                    p.id,
                    &p.url,
                    &client,
                    Some(config.share_gamification_stats),
                )
                .await;
            }
            _ => {
                // Peer unreachable — skip, preserve cached data
                tracing::debug!(
                    "Peer {} unreachable during leaderboard refresh, keeping cached stats",
                    p.url
                );
                continue;
            }
        }
    }

    // Now return the leaderboard (delegate to get_leaderboard logic)
    get_leaderboard(State(db)).await.into_response()
}

/// GET /api/gamification/leaderboard
/// Returns leaderboard combining local stats + peer stats, sorted by level then current.
pub async fn get_leaderboard(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::{installation_profile, library_config, peer_gamification_stats};

    // Check if network_gamification is enabled
    let profile = match installation_profile::Entity::find_by_id(1).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "Module not available"})),
            )
                .into_response();
        }
    };

    let enabled_modules: Vec<String> =
        serde_json::from_str(&profile.enabled_modules).unwrap_or_default();

    if !enabled_modules.contains(&"network_gamification".to_string()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Network gamification is disabled"})),
        )
            .into_response();
    }

    // Get local library name
    let library_name = match library_config::Entity::find_by_id(1).one(&db).await {
        Ok(Some(c)) => c.name,
        _ => "My Library".to_string(),
    };

    // Compute local stats
    let books_count = book::Entity::find().count(&db).await.unwrap_or(0) as i64;
    let read_count = book::Entity::find()
        .filter(book::Column::ReadingStatus.eq("read"))
        .count(&db)
        .await
        .unwrap_or(0) as i64;
    let loans_count = loan::Entity::find().count(&db).await.unwrap_or(0) as i64;
    let organized_count = count_catalogued_books(&db).await;

    let local_collector =
        calculate_track_progress(books_count, &COLLECTOR_THRESHOLDS, COLLECTOR_STEP);
    let local_reader = calculate_track_progress(read_count, &READER_THRESHOLDS, READER_STEP);
    let local_lender = calculate_track_progress(loans_count, &LENDER_THRESHOLDS, LENDER_STEP);
    let local_cataloguer =
        calculate_track_progress(organized_count, &CATALOGUER_THRESHOLDS, CATALOGUER_STEP);

    // Build leaderboard entries starting with local user
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

    // Get peer stats
    let peer_stats = peer_gamification_stats::Entity::find()
        .all(&db)
        .await
        .unwrap_or_default();

    // Freshness = oldest synced_at among peers (stalest peer = overall freshness)
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

    (
        StatusCode::OK,
        Json(LeaderboardResponse {
            collector: collector_entries,
            reader: reader_entries,
            lender: lender_entries,
            cataloguer: cataloguer_entries,
            last_refreshed,
        }),
    )
        .into_response()
}
