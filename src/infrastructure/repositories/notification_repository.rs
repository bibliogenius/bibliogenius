//! SeaORM implementation of NotificationRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};

use crate::domain::DomainError;
use crate::domain::notification_repository::{
    CreateNotification, MAX_NOTIFICATIONS, NotificationRepository, NotificationRow,
    TTL_GLOBAL_DAYS, TTL_READ_DAYS,
};
use crate::models::notification;

pub struct SeaOrmNotificationRepository {
    db: DatabaseConnection,
}

impl SeaOrmNotificationRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl From<notification::Model> for NotificationRow {
    fn from(m: notification::Model) -> Self {
        Self {
            id: m.id,
            event_type: m.event_type,
            category: m.category,
            title: m.title,
            body: m.body,
            ref_type: m.ref_type,
            ref_id: m.ref_id,
            read_at: m.read_at,
            created_at: m.created_at,
        }
    }
}

#[async_trait]
impl NotificationRepository for SeaOrmNotificationRepository {
    async fn create(&self, input: CreateNotification) -> Result<NotificationRow, DomainError> {
        let now = chrono::Utc::now().to_rfc3339();
        let category = input.event_type.category().as_str().to_string();

        let model = notification::ActiveModel {
            event_type: Set(input.event_type.as_str().to_string()),
            category: Set(category),
            title: Set(input.title),
            body: Set(input.body),
            ref_type: Set(input.ref_type),
            ref_id: Set(input.ref_id),
            read_at: Set(None),
            created_at: Set(now),
            ..Default::default()
        };

        let inserted = model.insert(&self.db).await?;

        // Fire-and-forget pruning after insert
        let _ = self.prune().await;

        Ok(inserted.into())
    }

    async fn list(
        &self,
        category: Option<&str>,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<NotificationRow>, DomainError> {
        let mut query = notification::Entity::find().order_by_desc(notification::Column::CreatedAt);

        if let Some(cat) = category {
            query = query.filter(notification::Column::Category.eq(cat));
        }

        let models: Vec<notification::Model> = query
            .offset(Some(offset))
            .limit(Some(limit))
            .all(&self.db)
            .await?;

        Ok(models.into_iter().map(NotificationRow::from).collect())
    }

    async fn unread_count(&self, category: Option<&str>) -> Result<i64, DomainError> {
        let mut query = notification::Entity::find().filter(notification::Column::ReadAt.is_null());

        if let Some(cat) = category {
            query = query.filter(notification::Column::Category.eq(cat));
        }

        Ok(query.count(&self.db).await? as i64)
    }

    async fn mark_read(&self, id: i32) -> Result<bool, DomainError> {
        let entry = notification::Entity::find_by_id(id).one(&self.db).await?;
        match entry {
            Some(row) => {
                let mut active: notification::ActiveModel = row.into();
                active.read_at = Set(Some(chrono::Utc::now().to_rfc3339()));
                active.update(&self.db).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn mark_all_read(&self) -> Result<i64, DomainError> {
        let now = chrono::Utc::now().to_rfc3339();
        let result = self
            .db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "UPDATE notifications SET read_at = $1 WHERE read_at IS NULL",
                [now.into()],
            ))
            .await?;
        Ok(result.rows_affected() as i64)
    }

    async fn dismiss(&self, id: i32) -> Result<bool, DomainError> {
        let result = notification::Entity::delete_by_id(id)
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected > 0)
    }

    async fn prune(&self) -> Result<i64, DomainError> {
        let mut total_deleted: u64 = 0;

        // 1. TTL global: delete anything older than TTL_GLOBAL_DAYS
        let cutoff_global =
            (chrono::Utc::now() - chrono::Duration::days(TTL_GLOBAL_DAYS)).to_rfc3339();
        let r = self
            .db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "DELETE FROM notifications WHERE created_at < $1",
                [cutoff_global.into()],
            ))
            .await?;
        total_deleted += r.rows_affected();

        // 2. TTL read: delete read items older than TTL_READ_DAYS after read_at
        let cutoff_read = (chrono::Utc::now() - chrono::Duration::days(TTL_READ_DAYS)).to_rfc3339();
        let r = self
            .db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "DELETE FROM notifications WHERE read_at IS NOT NULL AND read_at < $1",
                [cutoff_read.into()],
            ))
            .await?;
        total_deleted += r.rows_affected();

        // 3. Cap total: if still over MAX_NOTIFICATIONS, delete oldest (read first, then unread)
        let count = notification::Entity::find().count(&self.db).await?;
        if count > MAX_NOTIFICATIONS {
            let excess = count - MAX_NOTIFICATIONS;
            // Delete oldest read first
            let r = self
                .db
                .execute(Statement::from_sql_and_values(
                    self.db.get_database_backend(),
                    format!(
                        "DELETE FROM notifications WHERE id IN (
                            SELECT id FROM notifications WHERE read_at IS NOT NULL
                            ORDER BY created_at ASC LIMIT {}
                        )",
                        excess
                    ),
                    [],
                ))
                .await?;
            let deleted_read = r.rows_affected();
            total_deleted += deleted_read;

            // If still over cap, delete oldest unread
            if deleted_read < excess {
                let remaining = excess - deleted_read;
                let r = self
                    .db
                    .execute(Statement::from_sql_and_values(
                        self.db.get_database_backend(),
                        format!(
                            "DELETE FROM notifications WHERE id IN (
                                SELECT id FROM notifications
                                ORDER BY created_at ASC LIMIT {}
                            )",
                            remaining
                        ),
                        [],
                    ))
                    .await?;
                total_deleted += r.rows_affected();
            }
        }

        Ok(total_deleted as i64)
    }

    async fn exists(
        &self,
        event_type: &str,
        ref_type: &str,
        ref_id: &str,
    ) -> Result<bool, DomainError> {
        let count = notification::Entity::find()
            .filter(notification::Column::EventType.eq(event_type))
            .filter(notification::Column::RefType.eq(ref_type))
            .filter(notification::Column::RefId.eq(ref_id))
            .count(&self.db)
            .await?;
        Ok(count > 0)
    }
}
