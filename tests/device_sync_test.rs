//! Device Sync End-to-End Integration Tests
//!
//! Tests the full multi-device sync workflow:
//! - Local operations logged with source="local"
//! - Remote operations received via DeviceSyncService
//! - Echo prevention (remote ops not included in outbound sync)
//! - Safety mode review workflow (approve/reject)
//! - Approved ops picked up by processor (status: pending)

use std::sync::Arc;

use rust_lib_app::db;
use rust_lib_app::domain::LinkedDeviceRepository;
use rust_lib_app::infrastructure::SeaOrmLinkedDeviceRepository;
use rust_lib_app::models::operation_log;
use rust_lib_app::services::device_sync_service::{DeviceSyncService, RemoteOp};
use rust_lib_app::sync::{log_operation, log_remote_operation};
use sea_orm::{DatabaseConnection, EntityTrait};

async fn setup() -> (DatabaseConnection, DeviceSyncService) {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    let repo: Arc<dyn LinkedDeviceRepository> =
        Arc::new(SeaOrmLinkedDeviceRepository::new(db.clone()));
    let svc = DeviceSyncService::new(db.clone(), repo);
    (db, svc)
}

// ── Full sync flow ──────────────────────────────────────────────────

#[tokio::test]
async fn test_full_sync_flow_safety_off() {
    let (db, svc) = setup().await;

    // 1. Log some local operations
    log_operation(
        &db,
        "book",
        1,
        "INSERT",
        Some(serde_json::json!({"title": "Local Book"})),
    )
    .await
    .unwrap();
    log_operation(&db, "tag", 1, "INSERT", None).await.unwrap();

    // 2. Verify local ops are available for sync
    let local_ops = svc.get_local_ops_since(None).await.unwrap();
    assert_eq!(local_ops.len(), 2);

    // 3. Receive remote operations (safety OFF)
    let remote_ops = vec![
        RemoteOp {
            entity_type: "book".to_string(),
            entity_id: 100,
            operation: "INSERT".to_string(),
            payload: Some(serde_json::json!({"title": "Remote Book"})),
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        RemoteOp {
            entity_type: "contact".to_string(),
            entity_id: 50,
            operation: "INSERT".to_string(),
            payload: Some(serde_json::json!({"name": "Remote Contact"})),
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    ];

    let result = svc.receive_remote_ops(3, remote_ops, false).await.unwrap();
    assert_eq!(result.inserted_count, 2);

    // 4. Remote ops should have status "pending" (ready for processor)
    for id in &result.op_ids {
        let op = operation_log::Entity::find_by_id(*id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(op.status, "pending");
        assert_eq!(op.source, "device:3");
    }

    // 5. No pending review ops
    let pending_review = svc.get_pending_review_ops().await.unwrap();
    assert!(pending_review.is_empty());
}

#[tokio::test]
async fn test_full_sync_flow_safety_on() {
    let (db, svc) = setup().await;

    // 1. Receive remote operations with safety ON
    let remote_ops = vec![
        RemoteOp {
            entity_type: "book".to_string(),
            entity_id: 10,
            operation: "INSERT".to_string(),
            payload: Some(serde_json::json!({"title": "Needs Review"})),
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        RemoteOp {
            entity_type: "tag".to_string(),
            entity_id: 5,
            operation: "DELETE".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    ];

    let result = svc.receive_remote_ops(7, remote_ops, true).await.unwrap();
    assert_eq!(result.inserted_count, 2);

    // 2. Both ops should be pending_review
    let pending = svc.get_pending_review_ops().await.unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|op| op.status == "pending_review"));

    // 3. Approve the first, reject the second
    let approve_count = svc.approve_ops(&[result.op_ids[0]]).await.unwrap();
    assert_eq!(approve_count, 1);

    let reject_count = svc.reject_ops(&[result.op_ids[1]]).await.unwrap();
    assert_eq!(reject_count, 1);

    // 4. Verify statuses
    let op1 = operation_log::Entity::find_by_id(result.op_ids[0])
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op1.status, "pending", "Approved op should become pending");

    let op2 = operation_log::Entity::find_by_id(result.op_ids[1])
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op2.status, "skipped", "Rejected op should become skipped");

    // 5. No more pending_review
    let remaining = svc.get_pending_review_ops().await.unwrap();
    assert!(remaining.is_empty());
}

// ── Echo prevention ─────────────────────────────────────────────────

#[tokio::test]
async fn test_echo_prevention_remote_ops_not_in_outbound() {
    let (db, svc) = setup().await;

    // Log a local op
    log_operation(&db, "book", 1, "INSERT", None).await.unwrap();

    // Log a remote op (from device 5)
    log_remote_operation(&db, "book", 2, "INSERT", None, 5, false)
        .await
        .unwrap();

    // get_local_ops_since should only return the local op
    let outbound = svc.get_local_ops_since(None).await.unwrap();
    assert_eq!(outbound.len(), 1, "Only local ops should be in outbound");
    assert_eq!(outbound[0].entity_id, 1);
    assert_eq!(outbound[0].source, "local");
}

#[tokio::test]
async fn test_echo_prevention_with_timestamp_filter() {
    let (db, svc) = setup().await;

    // Log an old local op
    log_operation(&db, "book", 1, "INSERT", None).await.unwrap();

    // Small delay for timestamp ordering
    let cutoff = chrono::Utc::now().to_rfc3339();

    // Log a newer local op
    log_operation(&db, "tag", 1, "INSERT", None).await.unwrap();

    // Log a remote op (should be excluded regardless of timestamp)
    log_remote_operation(&db, "book", 99, "INSERT", None, 10, false)
        .await
        .unwrap();

    // Only newer local ops should be returned
    let outbound = svc.get_local_ops_since(Some(&cutoff)).await.unwrap();
    assert_eq!(outbound.len(), 1, "Should only return ops after cutoff");
    assert_eq!(outbound[0].entity_type, "tag");
}

// ── Bulk approve/reject ─────────────────────────────────────────────

#[tokio::test]
async fn test_approve_all_then_reject_all() {
    let (_db, svc) = setup().await;

    // Add 3 ops with safety on
    let ops = vec![
        RemoteOp {
            entity_type: "book".to_string(),
            entity_id: 1,
            operation: "INSERT".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        RemoteOp {
            entity_type: "book".to_string(),
            entity_id: 2,
            operation: "INSERT".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        RemoteOp {
            entity_type: "book".to_string(),
            entity_id: 3,
            operation: "INSERT".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    ];

    svc.receive_remote_ops(1, ops, true).await.unwrap();
    assert_eq!(svc.get_pending_review_ops().await.unwrap().len(), 3);

    // Approve all
    let approved = svc.approve_all_pending_review().await.unwrap();
    assert_eq!(approved, 3);
    assert!(svc.get_pending_review_ops().await.unwrap().is_empty());

    // Add 2 more
    let ops2 = vec![
        RemoteOp {
            entity_type: "tag".to_string(),
            entity_id: 1,
            operation: "DELETE".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        RemoteOp {
            entity_type: "tag".to_string(),
            entity_id: 2,
            operation: "DELETE".to_string(),
            payload: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    ];

    svc.receive_remote_ops(1, ops2, true).await.unwrap();

    // Reject all
    let rejected = svc.reject_all_pending_review().await.unwrap();
    assert_eq!(rejected, 2);
    assert!(svc.get_pending_review_ops().await.unwrap().is_empty());
}

// ── Edge cases ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_approve_empty_ids_returns_zero() {
    let (_db, svc) = setup().await;
    let count = svc.approve_ops(&[]).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_reject_nonexistent_ids_returns_zero() {
    let (_db, svc) = setup().await;
    let count = svc.reject_ops(&[9999, 8888]).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_approve_already_approved_returns_zero() {
    let (db, svc) = setup().await;

    let id = log_remote_operation(&db, "book", 1, "INSERT", None, 5, true)
        .await
        .unwrap();

    // Approve once
    let first = svc.approve_ops(&[id]).await.unwrap();
    assert_eq!(first, 1);

    // Approve again (already pending, not pending_review)
    let second = svc.approve_ops(&[id]).await.unwrap();
    assert_eq!(second, 0, "Already approved ops should not be re-approved");
}

#[tokio::test]
async fn test_mixed_sources_in_log() {
    let (db, _svc) = setup().await;

    // Insert various sources
    log_operation(&db, "book", 1, "INSERT", None).await.unwrap();
    log_remote_operation(&db, "book", 2, "INSERT", None, 3, false)
        .await
        .unwrap();
    log_remote_operation(&db, "book", 3, "INSERT", None, 7, true)
        .await
        .unwrap();

    let all = operation_log::Entity::find().all(&db).await.unwrap();
    assert_eq!(all.len(), 3);

    let local_count = all.iter().filter(|op| op.source == "local").count();
    let device3 = all.iter().filter(|op| op.source == "device:3").count();
    let device7 = all.iter().filter(|op| op.source == "device:7").count();

    assert_eq!(local_count, 1);
    assert_eq!(device3, 1);
    assert_eq!(device7, 1);
}

#[tokio::test]
async fn test_receive_empty_ops_returns_zero() {
    let (_db, svc) = setup().await;
    let result = svc.receive_remote_ops(1, vec![], false).await.unwrap();
    assert_eq!(result.inserted_count, 0);
    assert!(result.op_ids.is_empty());
}
