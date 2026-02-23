//! SeaORM implementation of GamificationRepository

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set,
};

use crate::domain::{
    DomainError, GamificationConfigRow, GamificationConfigUpdate, GamificationRepository,
    PeerGamificationStatsRow,
};
use crate::models::{
    book, gamification_achievements, gamification_config, gamification_streaks,
    installation_profile, library_config, loan, peer_gamification_stats,
};

/// SeaORM-based implementation of GamificationRepository
pub struct SeaOrmGamificationRepository {
    db: DatabaseConnection,
}

impl SeaOrmGamificationRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl GamificationRepository for SeaOrmGamificationRepository {
    async fn count_books(&self) -> Result<i64, DomainError> {
        Ok(book::Entity::find().count(&self.db).await? as i64)
    }

    async fn count_books_read(&self) -> Result<i64, DomainError> {
        Ok(book::Entity::find()
            .filter(book::Column::ReadingStatus.eq("read"))
            .count(&self.db)
            .await? as i64)
    }

    async fn count_books_read_in_year(&self, year: &str) -> Result<i64, DomainError> {
        Ok(book::Entity::find()
            .filter(book::Column::FinishedReadingAt.like(format!("{}%", year)))
            .count(&self.db)
            .await? as i64)
    }

    async fn count_loans(&self) -> Result<i64, DomainError> {
        Ok(loan::Entity::find().count(&self.db).await? as i64)
    }

    async fn count_catalogued_books(&self) -> Result<i64, DomainError> {
        // Books are tagged via the `subjects` JSON column (e.g. '["classique","littérature"]'),
        // NOT via the `book_tags` junction table (which is unused).
        // Count books that have at least one subject assigned.
        Ok(book::Entity::find()
            .filter(book::Column::Subjects.is_not_null())
            .filter(book::Column::Subjects.ne(""))
            .filter(book::Column::Subjects.ne("[]"))
            .filter(book::Column::Subjects.ne("null"))
            .count(&self.db)
            .await? as i64)
    }

    async fn get_streak(
        &self,
        user_id: i32,
    ) -> Result<Option<(i32, i32, Option<String>)>, DomainError> {
        let streak = gamification_streaks::Entity::find()
            .filter(gamification_streaks::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?;

        Ok(streak.map(|s| (s.current_streak, s.longest_streak, s.last_activity_date)))
    }

    async fn update_streak(
        &self,
        user_id: i32,
        current: i32,
        longest: i32,
        last_date: &str,
    ) -> Result<(), DomainError> {
        // Try to find existing streak
        let existing = gamification_streaks::Entity::find()
            .filter(gamification_streaks::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?;

        if let Some(model) = existing {
            let mut active: gamification_streaks::ActiveModel = model.into();
            active.current_streak = Set(current);
            active.longest_streak = Set(longest);
            active.last_activity_date = Set(Some(last_date.to_string()));
            active.update(&self.db).await?;
        } else {
            let new_streak = gamification_streaks::ActiveModel {
                user_id: Set(user_id),
                current_streak: Set(current),
                longest_streak: Set(longest),
                last_activity_date: Set(Some(last_date.to_string())),
                ..Default::default()
            };
            new_streak.insert(&self.db).await?;
        }

        Ok(())
    }

    async fn get_recent_achievements(
        &self,
        user_id: i32,
        limit: u32,
    ) -> Result<Vec<String>, DomainError> {
        let achievements = gamification_achievements::Entity::find()
            .filter(gamification_achievements::Column::UserId.eq(user_id))
            .order_by_desc(gamification_achievements::Column::UnlockedAt)
            .all(&self.db)
            .await?;

        Ok(achievements
            .into_iter()
            .take(limit as usize)
            .map(|a| a.achievement_id)
            .collect())
    }

    async fn unlock_achievement(
        &self,
        user_id: i32,
        achievement_id: &str,
    ) -> Result<bool, DomainError> {
        // Check if already unlocked
        let existing = gamification_achievements::Entity::find()
            .filter(gamification_achievements::Column::UserId.eq(user_id))
            .filter(gamification_achievements::Column::AchievementId.eq(achievement_id))
            .one(&self.db)
            .await?;

        if existing.is_some() {
            return Ok(false); // Already unlocked
        }

        let new_achievement = gamification_achievements::ActiveModel {
            user_id: Set(user_id),
            achievement_id: Set(achievement_id.to_string()),
            unlocked_at: Set(Utc::now().to_rfc3339()),
            ..Default::default()
        };
        new_achievement.insert(&self.db).await?;

        Ok(true) // Newly unlocked
    }

    async fn get_config(&self, user_id: i32) -> Result<Option<GamificationConfigRow>, DomainError> {
        let config = gamification_config::Entity::find()
            .filter(gamification_config::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?;

        Ok(config.map(|c| GamificationConfigRow {
            achievements_style: c.achievements_style,
            reading_goal_yearly: c.reading_goal_yearly,
        }))
    }

    async fn update_config(
        &self,
        user_id: i32,
        config: GamificationConfigUpdate,
    ) -> Result<(), DomainError> {
        let existing = gamification_config::Entity::find()
            .filter(gamification_config::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?;

        if let Some(model) = existing {
            let mut active: gamification_config::ActiveModel = model.into();
            if let Some(goal) = config.reading_goal_yearly {
                active.reading_goal_yearly = Set(goal);
            }
            if let Some(style) = config.achievements_style {
                active.achievements_style = Set(style);
            }
            active.updated_at = Set(Utc::now().to_rfc3339());
            active.update(&self.db).await?;
        } else {
            // Create default config with overrides
            let now = Utc::now().to_rfc3339();
            let new_config = gamification_config::ActiveModel {
                user_id: Set(user_id),
                preset: Set("individual".to_string()),
                streaks_enabled: Set(true),
                achievements_enabled: Set(true),
                achievements_style: Set(config.achievements_style.unwrap_or("minimal".to_string())),
                reading_goals_enabled: Set(true),
                reading_goal_yearly: Set(config.reading_goal_yearly.unwrap_or(12)),
                tracks_enabled: Set(r#"["collector","reader","lender","cataloguer"]"#.to_string()),
                notifications_enabled: Set(true),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            new_config.insert(&self.db).await?;
        }

        Ok(())
    }

    async fn get_peer_stats(&self) -> Result<Vec<PeerGamificationStatsRow>, DomainError> {
        let stats = peer_gamification_stats::Entity::find()
            .all(&self.db)
            .await?;

        Ok(stats
            .into_iter()
            .map(|s| PeerGamificationStatsRow {
                peer_id: s.peer_id,
                library_name: s.library_name,
                collector_level: s.collector_level,
                collector_current: s.collector_current,
                reader_level: s.reader_level,
                reader_current: s.reader_current,
                lender_level: s.lender_level,
                lender_current: s.lender_current,
                cataloguer_level: s.cataloguer_level,
                cataloguer_current: s.cataloguer_current,
                synced_at: s.synced_at,
            })
            .collect())
    }

    async fn is_module_enabled(&self, module: &str) -> Result<bool, DomainError> {
        let profile = installation_profile::Entity::find_by_id(1)
            .one(&self.db)
            .await?;

        match profile {
            Some(p) => {
                let modules: Vec<String> =
                    serde_json::from_str(&p.enabled_modules).unwrap_or_default();
                Ok(modules.contains(&module.to_string()))
            }
            None => Ok(false),
        }
    }

    async fn get_library_name(&self) -> Result<String, DomainError> {
        let config = library_config::Entity::find_by_id(1).one(&self.db).await?;

        Ok(config.map(|c| c.name).unwrap_or("My Library".to_string()))
    }
}
