//! Device sync service for multi-device sync (ADR-011).
//!
//! Orchestrates operation log exchange between paired devices:
//! - Collects local ops since a given timestamp for outbound sync
//! - Receives remote ops and inserts them with appropriate status
//! - Manages the review workflow (approve/reject) when sync safety is enabled

use std::sync::Arc;

use sea_orm::prelude::Expr;
use sea_orm::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::LinkedDeviceRepository;
use crate::models::operation_log;
use crate::sync::log_remote_operation;

/// A single operation received from a remote device during sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteOp {
    pub entity_type: String,
    pub entity_id: i32,
    pub operation: String,
    pub payload: Option<Value>,
    pub created_at: String,
}

/// Result of receiving remote operations
#[derive(Debug, Clone, Serialize)]
pub struct SyncReceiveResult {
    pub inserted_count: u32,
    pub op_ids: Vec<i32>,
}

/// Service managing operation log synchronization between linked devices
pub struct DeviceSyncService {
    db: DatabaseConnection,
    linked_device_repo: Arc<dyn LinkedDeviceRepository>,
}

impl DeviceSyncService {
    pub fn new(
        db: DatabaseConnection,
        linked_device_repo: Arc<dyn LinkedDeviceRepository>,
    ) -> Self {
        Self {
            db,
            linked_device_repo,
        }
    }

    /// Fetch all local operations since a given timestamp.
    /// Only returns ops with source = "local" (echo prevention).
    /// If `since` is None, returns all local ops.
    pub async fn get_local_ops_since(
        &self,
        since: Option<&str>,
    ) -> Result<Vec<operation_log::Model>, DbErr> {
        let mut query =
            operation_log::Entity::find().filter(operation_log::Column::Source.eq("local"));

        if let Some(since_ts) = since {
            query = query.filter(operation_log::Column::CreatedAt.gt(since_ts));
        }

        query
            .order_by_asc(operation_log::Column::CreatedAt)
            .all(&self.db)
            .await
    }

    /// Receive operations from a remote device.
    /// Inserts each op into the operation_log with the appropriate status:
    /// - "pending_review" when safety mode is on (user must approve)
    /// - "pending" when safety mode is off (processor auto-applies)
    pub async fn receive_remote_ops(
        &self,
        device_id: i32,
        ops: Vec<RemoteOp>,
        safety_mode: bool,
    ) -> Result<SyncReceiveResult, String> {
        let mut op_ids = Vec::new();

        for op in &ops {
            let id = log_remote_operation(
                &self.db,
                &op.entity_type,
                op.entity_id,
                &op.operation,
                op.payload.clone(),
                device_id,
                safety_mode,
            )
            .await
            .map_err(|e| format!("Failed to log remote op: {e}"))?;

            op_ids.push(id);
        }

        Ok(SyncReceiveResult {
            inserted_count: op_ids.len() as u32,
            op_ids,
        })
    }

    /// Fetch all operations with status "pending_review" (awaiting user approval).
    pub async fn get_pending_review_ops(&self) -> Result<Vec<operation_log::Model>, DbErr> {
        operation_log::Entity::find()
            .filter(operation_log::Column::Status.eq("pending_review"))
            .order_by_asc(operation_log::Column::CreatedAt)
            .all(&self.db)
            .await
    }

    /// Approve specific operations by ID.
    /// Changes status from "pending_review" to "pending" so the processor picks them up.
    pub async fn approve_ops(&self, ids: &[i32]) -> Result<u32, DbErr> {
        if ids.is_empty() {
            return Ok(0);
        }

        let result = operation_log::Entity::update_many()
            .col_expr(
                operation_log::Column::Status,
                Expr::value("pending".to_owned()),
            )
            .filter(operation_log::Column::Id.is_in(ids.to_vec()))
            .filter(operation_log::Column::Status.eq("pending_review"))
            .exec(&self.db)
            .await?;

        Ok(result.rows_affected as u32)
    }

    /// Reject specific operations by ID.
    /// Changes status from "pending_review" to "skipped".
    pub async fn reject_ops(&self, ids: &[i32]) -> Result<u32, DbErr> {
        if ids.is_empty() {
            return Ok(0);
        }

        let result = operation_log::Entity::update_many()
            .col_expr(
                operation_log::Column::Status,
                Expr::value("skipped".to_owned()),
            )
            .filter(operation_log::Column::Id.is_in(ids.to_vec()))
            .filter(operation_log::Column::Status.eq("pending_review"))
            .exec(&self.db)
            .await?;

        Ok(result.rows_affected as u32)
    }

    /// Approve all pending_review operations at once.
    pub async fn approve_all_pending_review(&self) -> Result<u32, DbErr> {
        let result = operation_log::Entity::update_many()
            .col_expr(
                operation_log::Column::Status,
                Expr::value("pending".to_owned()),
            )
            .filter(operation_log::Column::Status.eq("pending_review"))
            .exec(&self.db)
            .await?;

        Ok(result.rows_affected as u32)
    }

    /// Reject all pending_review operations at once.
    pub async fn reject_all_pending_review(&self) -> Result<u32, DbErr> {
        let result = operation_log::Entity::update_many()
            .col_expr(
                operation_log::Column::Status,
                Expr::value("skipped".to_owned()),
            )
            .filter(operation_log::Column::Status.eq("pending_review"))
            .exec(&self.db)
            .await?;

        Ok(result.rows_affected as u32)
    }

    /// Update the last_synced timestamp on a linked device.
    pub async fn update_device_last_synced(
        &self,
        device_id: i32,
        timestamp: &str,
    ) -> Result<(), String> {
        self.linked_device_repo
            .update_last_synced(device_id, timestamp)
            .await
            .map_err(|e| format!("Failed to update last_synced: {e}"))
    }

    /// Get the database connection (needed by handlers for e2ee dispatch).
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::SeaOrmLinkedDeviceRepository;
    use crate::infrastructure::db::init_db;

    async fn setup() -> (DatabaseConnection, DeviceSyncService) {
        let db = init_db("sqlite::memory:").await.unwrap();
        let repo: Arc<dyn LinkedDeviceRepository> =
            Arc::new(SeaOrmLinkedDeviceRepository::new(db.clone()));
        let svc = DeviceSyncService::new(db.clone(), repo);
        (db, svc)
    }

    #[tokio::test]
    async fn test_get_local_ops_since_filters_by_source() {
        let (db, svc) = setup().await;

        // Insert a local op
        let _ = crate::sync::log_operation(&db, "book", 1, "INSERT", None).await;

        // Insert a remote op
        let _ = log_remote_operation(&db, "book", 2, "INSERT", None, 99, false).await;

        // get_local_ops_since should only return the local op
        let ops = svc.get_local_ops_since(None).await.unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].entity_id, 1);
        assert_eq!(ops[0].source, "local");
    }

    #[tokio::test]
    async fn test_receive_remote_ops_safety_on() {
        let (_db, svc) = setup().await;

        let ops = vec![
            RemoteOp {
                entity_type: "book".to_string(),
                entity_id: 10,
                operation: "INSERT".to_string(),
                payload: Some(serde_json::json!({"title": "Test"})),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
            RemoteOp {
                entity_type: "tag".to_string(),
                entity_id: 5,
                operation: "INSERT".to_string(),
                payload: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        ];

        let result = svc.receive_remote_ops(3, ops, true).await.unwrap();
        assert_eq!(result.inserted_count, 2);
        assert_eq!(result.op_ids.len(), 2);

        // Verify they have pending_review status
        let pending = svc.get_pending_review_ops().await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].status, "pending_review");
        assert!(pending[0].source.starts_with("device:"));
    }

    #[tokio::test]
    async fn test_receive_remote_ops_safety_off() {
        let (db, svc) = setup().await;

        let ops = vec![RemoteOp {
            entity_type: "contact".to_string(),
            entity_id: 7,
            operation: "INSERT".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        }];

        let result = svc.receive_remote_ops(2, ops, false).await.unwrap();
        assert_eq!(result.inserted_count, 1);

        // Should be "pending" (not "pending_review")
        let op = operation_log::Entity::find_by_id(result.op_ids[0])
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(op.status, "pending");
        assert_eq!(op.source, "device:2");
    }

    #[tokio::test]
    async fn test_approve_ops_changes_status() {
        let (db, svc) = setup().await;

        // Insert a pending_review op
        let id = log_remote_operation(&db, "book", 1, "INSERT", None, 5, true)
            .await
            .unwrap();

        // Approve it
        let count = svc.approve_ops(&[id]).await.unwrap();
        assert_eq!(count, 1);

        // Verify status changed to "pending"
        let op = operation_log::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(op.status, "pending");
    }

    #[tokio::test]
    async fn test_reject_ops_changes_status() {
        let (db, svc) = setup().await;

        let id = log_remote_operation(&db, "tag", 3, "DELETE", None, 5, true)
            .await
            .unwrap();

        let count = svc.reject_ops(&[id]).await.unwrap();
        assert_eq!(count, 1);

        let op = operation_log::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(op.status, "skipped");
    }

    #[tokio::test]
    async fn test_approve_all_and_reject_all() {
        let (db, svc) = setup().await;

        // Insert 3 pending_review ops
        let _ = log_remote_operation(&db, "book", 1, "INSERT", None, 5, true).await;
        let _ = log_remote_operation(&db, "book", 2, "INSERT", None, 5, true).await;
        let _ = log_remote_operation(&db, "tag", 1, "INSERT", None, 5, true).await;

        // Approve all
        let count = svc.approve_all_pending_review().await.unwrap();
        assert_eq!(count, 3);

        // No more pending_review
        let pending = svc.get_pending_review_ops().await.unwrap();
        assert!(pending.is_empty());

        // Insert 2 more for reject test
        let _ = log_remote_operation(&db, "contact", 1, "DELETE", None, 5, true).await;
        let _ = log_remote_operation(&db, "contact", 2, "DELETE", None, 5, true).await;

        let count = svc.reject_all_pending_review().await.unwrap();
        assert_eq!(count, 2);

        let pending = svc.get_pending_review_ops().await.unwrap();
        assert!(pending.is_empty());
    }
}
