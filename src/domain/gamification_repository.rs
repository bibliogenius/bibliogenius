//! Gamification repository trait and related types
//!
//! Domain-level abstractions for gamification data access.
//! No framework dependencies (no SeaORM, no Axum).

use async_trait::async_trait;

use super::DomainError;

/// Gamification config row (read from DB)
#[derive(Debug, Clone)]
pub struct GamificationConfigRow {
    pub achievements_style: String,
    pub reading_goal_yearly: i32,
}

/// Gamification config update (partial)
#[derive(Debug, Clone, Default)]
pub struct GamificationConfigUpdate {
    pub reading_goal_yearly: Option<i32>,
    pub achievements_style: Option<String>,
}

/// Peer gamification stats row (for leaderboard)
#[derive(Debug, Clone)]
pub struct PeerGamificationStatsRow {
    pub peer_id: i32,
    pub library_name: String,
    pub collector_level: i32,
    pub collector_current: i32,
    pub reader_level: i32,
    pub reader_current: i32,
    pub lender_level: i32,
    pub lender_current: i32,
    pub cataloguer_level: i32,
    pub cataloguer_current: i32,
    pub synced_at: String,
}

/// Repository trait for Gamification
#[async_trait]
pub trait GamificationRepository: Send + Sync {
    /// Count all books in library
    async fn count_books(&self) -> Result<i64, DomainError>;

    /// Count books with reading_status = 'read'
    async fn count_books_read(&self) -> Result<i64, DomainError>;

    /// Count books finished in a given year (finished_reading_at LIKE 'YYYY%')
    async fn count_books_read_in_year(&self, year: &str) -> Result<i64, DomainError>;

    /// Count all loans
    async fn count_loans(&self) -> Result<i64, DomainError>;

    /// Count distinct books assigned to at least one tag/shelf
    async fn count_catalogued_books(&self) -> Result<i64, DomainError>;

    /// Get streak info for user: (current_streak, longest_streak, last_activity_date)
    async fn get_streak(
        &self,
        user_id: i32,
    ) -> Result<Option<(i32, i32, Option<String>)>, DomainError>;

    /// Update streak for user (upsert)
    async fn update_streak(
        &self,
        user_id: i32,
        current: i32,
        longest: i32,
        last_date: &str,
    ) -> Result<(), DomainError>;

    /// Get recent achievement IDs (ordered by unlocked_at DESC, limit N)
    async fn get_recent_achievements(
        &self,
        user_id: i32,
        limit: u32,
    ) -> Result<Vec<String>, DomainError>;

    /// Unlock an achievement (idempotent — returns true if newly unlocked)
    async fn unlock_achievement(
        &self,
        user_id: i32,
        achievement_id: &str,
    ) -> Result<bool, DomainError>;

    /// Get gamification config for user
    async fn get_config(&self, user_id: i32) -> Result<Option<GamificationConfigRow>, DomainError>;

    /// Update gamification config for user
    async fn update_config(
        &self,
        user_id: i32,
        config: GamificationConfigUpdate,
    ) -> Result<(), DomainError>;

    /// Get all peer gamification stats (for leaderboard)
    async fn get_peer_stats(&self) -> Result<Vec<PeerGamificationStatsRow>, DomainError>;

    /// Check if a module is enabled in installation profile
    async fn is_module_enabled(&self, module: &str) -> Result<bool, DomainError>;

    /// Get library name from library_config
    async fn get_library_name(&self) -> Result<String, DomainError>;
}
