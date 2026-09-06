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
        // DISTINCT on the one column that is needed. Reading whole models here
        // pulled the entire table (payloads included) into memory, and it
        // failed outright on installs created before the uuid switch: their
        // `entity_id` column is declared INTEGER, holds numeric ids next to
        // uuid strings, and sqlx refuses INTEGER -> String. The filter menu
        // then came back empty on exactly the libraries with the most history.
        OperationLog::find()
            .select_only()
            .column(operation_log::Column::EntityType)
            .distinct()
            .order_by_asc(operation_log::Column::EntityType)
            .into_tuple::<String>()
            .all(self.db)
            .await
            .map_err(|e| DomainError::Database(e.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// Rebuild `operation_log` the way installs predating the uuid switch
    /// declare it: `entity_id INTEGER`. That affinity is what lets numeric ids
    /// and uuid strings coexist as different storage classes in one column,
    /// and it never goes away on its own once the table exists. The table the
    /// migrations just created is renamed aside rather than removed: this is a
    /// throwaway in-memory database and nothing reads the leftover.
    async fn setup_legacy_schema() -> DatabaseConnection {
        let db = db::init_db("sqlite::memory:")
            .await
            .expect("init_db in memory");
        for sql in [
            "ALTER TABLE operation_log RENAME TO operation_log_modern_shape",
            r#"CREATE TABLE operation_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entity_type TEXT NOT NULL,
                entity_id INTEGER NOT NULL,
                operation TEXT NOT NULL,
                payload TEXT,
                created_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                error_message TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                source TEXT NOT NULL DEFAULT 'local'
            )"#,
            // Pre-uuid row: entity_id lands as INTEGER storage.
            "INSERT INTO operation_log (entity_type, entity_id, operation, created_at, status)
             VALUES ('book', 42, 'INSERT', '2026-06-22T10:00:00+00:00', 'applied')",
            // Uuid-era row: TEXT cannot be coerced, so it stays TEXT.
            "INSERT INTO operation_log (entity_type, entity_id, operation, created_at, status)
             VALUES ('collection', '8f14e45f-ceea-467a-9b2c-0a1b2c3d4e5f', 'DELETE',
                     '2026-08-24T10:00:00+00:00', 'applied')",
        ] {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                sql.to_owned(),
            ))
            .await
            .expect("legacy schema fixture");
        }
        db
    }

    #[tokio::test]
    async fn entity_types_survive_pre_uuid_integer_ids() {
        let db = setup_legacy_schema().await;
        let repo = SeaOrmOperationLogViewerRepository::new(&db);

        let types = repo
            .get_entity_types()
            .await
            .expect("listing entity types must not depend on how entity_id is stored");

        // Reading whole models here used to fail on the INTEGER row, which left
        // the viewer's entity filter with no options at all.
        assert_eq!(types, vec!["book".to_string(), "collection".to_string()]);
    }
}
