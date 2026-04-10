//! Loan settings repository trait

use async_trait::async_trait;

use super::DomainError;

/// Loan settings (global configuration)
#[derive(Debug, Clone)]
pub struct LoanSettings {
    pub default_loan_duration_days: i32,
    pub per_book_duration_enabled: bool,
    pub reminder_days_before_due: i32,
}

/// Repository trait for loan duration settings
#[async_trait]
pub trait LoanSettingsRepository: Send + Sync {
    /// Get the current loan settings
    async fn get_settings(&self) -> Result<LoanSettings, DomainError>;

    /// Update the global loan settings
    async fn update_settings(&self, settings: LoanSettings) -> Result<LoanSettings, DomainError>;

    /// Get the per-book loan duration override (None = use global default)
    async fn get_book_loan_duration(&self, book_id: i32) -> Result<Option<i32>, DomainError>;

    /// Set the per-book loan duration override (None = clear, use global default)
    async fn set_book_loan_duration(
        &self,
        book_id: i32,
        days: Option<i32>,
    ) -> Result<(), DomainError>;

    /// Get the effective loan duration for a book:
    /// per-book value (if per_book_duration_enabled and set), else global default.
    async fn get_effective_duration(&self, book_id: i32) -> Result<i32, DomainError>;
}
