//! SeaORM implementation of the operation log viewer repository

use async_trait::async_trait;
use sea_orm::*;

use crate::domain::DomainError;
use crate::models::operation_log::{self, Entity as OperationLog};

use super::domain::{
    OperationLogEntry, OperationLogFilter, OperationLogPage, OperationLogStats,
    OperationLogViewerRepository,
};

pub struct SeaOrmOperationLogViewerRepository<'a> {
    db: &'a DatabaseConnection,
}

impl<'a> SeaOrmOperationLogViewerRepository<'a> {
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl OperationLogViewerRepository for SeaOrmOperationLogViewerRepository<'_> {
    async fn find_all(&self, filter: OperationLogFilter) -> Result<OperationLogPage, DomainError> {
        let mut query = OperationLog::find();

        if let Some(ref et) = filter.entity_type {
            query = query.filter(operation_log::Column::EntityType.eq(et));
        }
        if let Some(ref op) = filter.operation {
            query = query.filter(operation_log::Column::Operation.eq(op));
        }
        if let Some(ref st) = filter.status {
            query = query.filter(operation_log::Column::Status.eq(st));
        }
        if let Some(ref q) = filter.query {
            let pattern = format!("%{q}%");
            query = query.filter(
                Condition::any()
                    .add(operation_log::Column::EntityType.like(&pattern))
                    .add(operation_log::Column::Operation.like(&pattern))
                    .add(operation_log::Column::Payload.like(&pattern)),
            );
        }
        if let Some(ref since) = filter.since {
            query = query.filter(operation_log::Column::CreatedAt.gte(since));
        }
        if let Some(ref until) = filter.until {
            query = query.filter(operation_log::Column::CreatedAt.lte(until));
        }

        let total = query
            .clone()
            .count(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        let entries = query
            .order_by_desc(operation_log::Column::Id)
            .offset(filter.page * filter.limit)
            .limit(filter.limit)
            .all(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        Ok(OperationLogPage {
            entries: entries.into_iter().map(|m| m.into()).collect(),
            total,
            page: filter.page,
            limit: filter.limit,
        })
    }

    async fn get_stats(&self) -> Result<OperationLogStats, DomainError> {
        let total = OperationLog::find()
            .count(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        let today_str = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let today = OperationLog::find()
            .filter(operation_log::Column::CreatedAt.starts_with(&today_str))
            .count(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        let pending = OperationLog::find()
            .filter(operation_log::Column::Status.eq("pending"))
            .count(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        let failed = OperationLog::find()
            .filter(operation_log::Column::Status.eq("failed"))
            .count(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        Ok(OperationLogStats {
            total,
            today,
            pending,
            failed,
        })
    }

    async fn get_entity_types(&self) -> Result<Vec<String>, DomainError> {
        // Use raw SQL for DISTINCT since SeaORM doesn't have a clean API for it
        let rows: Vec<operation_log::Model> = OperationLog::find()
            .all(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))?;

        let mut types: Vec<String> = rows.iter().map(|r| r.entity_type.clone()).collect();
        types.sort();
        types.dedup();
        Ok(types)
    }
}

impl From<operation_log::Model> for OperationLogEntry {
    fn from(m: operation_log::Model) -> Self {
        Self {
            id: m.id,
            entity_type: m.entity_type,
            entity_id: m.entity_id,
            operation: m.operation,
            payload: m.payload,
            status: m.status,
            error_message: m.error_message,
            pinned: m.pinned != 0,
            created_at: m.created_at,
        }
    }
}
