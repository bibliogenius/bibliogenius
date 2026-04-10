//! SeaORM implementation of LoanSettingsRepository

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};

use crate::domain::{DomainError, LoanSettings, LoanSettingsRepository};
use crate::models::book;

/// SeaORM-based implementation of LoanSettingsRepository
pub struct SeaOrmLoanSettingsRepository {
    db: DatabaseConnection,
}

impl SeaOrmLoanSettingsRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl LoanSettingsRepository for SeaOrmLoanSettingsRepository {
    async fn get_settings(&self) -> Result<LoanSettings, DomainError> {
        let row = self
            .db
            .query_one(Statement::from_string(
                self.db.get_database_backend(),
                "SELECT default_loan_duration_days, per_book_duration_enabled, reminder_days_before_due FROM loan_settings WHERE id = 1".to_owned(),
            ))
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?
            .ok_or(DomainError::NotFound)?;

        let days: i32 = row
            .try_get_by_index(0)
            .map_err(|e| DomainError::Database(e.to_string()))?;
        let per_book: bool = row
            .try_get_by_index::<i32>(1)
            .map(|v| v != 0)
            .map_err(|e| DomainError::Database(e.to_string()))?;
        let reminder_days: i32 = row
            .try_get_by_index(2)
            .map_err(|e| DomainError::Database(e.to_string()))?;

        Ok(LoanSettings {
            default_loan_duration_days: days,
            per_book_duration_enabled: per_book,
            reminder_days_before_due: reminder_days,
        })
    }

    async fn update_settings(&self, settings: LoanSettings) -> Result<LoanSettings, DomainError> {
        let days = settings.default_loan_duration_days.clamp(1, 365);
        let per_book = if settings.per_book_duration_enabled {
            1
        } else {
            0
        };
        let reminder = settings.reminder_days_before_due.clamp(1, 10);

        self.db
            .execute(Statement::from_string(
                self.db.get_database_backend(),
                format!(
                    "UPDATE loan_settings SET default_loan_duration_days = {}, per_book_duration_enabled = {}, reminder_days_before_due = {} WHERE id = 1",
                    days, per_book, reminder,
                ),
            ))
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        Ok(LoanSettings {
            default_loan_duration_days: days,
            per_book_duration_enabled: settings.per_book_duration_enabled,
            reminder_days_before_due: reminder,
        })
    }

    async fn get_book_loan_duration(&self, book_id: i32) -> Result<Option<i32>, DomainError> {
        let book = book::Entity::find_by_id(book_id)
            .one(&self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?
            .ok_or(DomainError::NotFound)?;

        Ok(book.loan_duration_days)
    }

    async fn set_book_loan_duration(
        &self,
        book_id: i32,
        days: Option<i32>,
    ) -> Result<(), DomainError> {
        let clamped = days.map(|d| d.clamp(1, 365));

        let book = book::Entity::find_by_id(book_id)
            .one(&self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?
            .ok_or(DomainError::NotFound)?;

        let mut active: book::ActiveModel = book.into();
        active.loan_duration_days = Set(clamped);
        ActiveModelTrait::update(active, &self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        Ok(())
    }

    async fn get_effective_duration(&self, book_id: i32) -> Result<i32, DomainError> {
        let settings = self.get_settings().await?;

        if settings.per_book_duration_enabled
            && let Ok(Some(days)) = self.get_book_loan_duration(book_id).await
        {
            return Ok(days);
        }

        Ok(settings.default_loan_duration_days)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::LoanSettingsRepository;
    use sea_orm::Database;

    async fn setup_test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db)
            .await
            .unwrap();
        db
    }

    #[tokio::test]
    async fn test_get_default_settings() {
        let db = setup_test_db().await;
        let repo = SeaOrmLoanSettingsRepository::new(db);

        let settings = repo.get_settings().await.unwrap();
        assert_eq!(settings.default_loan_duration_days, 21);
        assert!(!settings.per_book_duration_enabled);
        assert_eq!(settings.reminder_days_before_due, 2);
    }

    #[tokio::test]
    async fn test_update_and_read_back() {
        let db = setup_test_db().await;
        let repo = SeaOrmLoanSettingsRepository::new(db);

        // Update to 40 days, reminder 5 days
        let updated = repo
            .update_settings(LoanSettings {
                default_loan_duration_days: 40,
                per_book_duration_enabled: true,
                reminder_days_before_due: 5,
            })
            .await
            .unwrap();
        assert_eq!(updated.default_loan_duration_days, 40);
        assert!(updated.per_book_duration_enabled);
        assert_eq!(updated.reminder_days_before_due, 5);

        // Read back -- must be 40/true/5, not 21/false/2
        let reloaded = repo.get_settings().await.unwrap();
        assert_eq!(reloaded.default_loan_duration_days, 40);
        assert!(reloaded.per_book_duration_enabled);
        assert_eq!(reloaded.reminder_days_before_due, 5);
    }
}
