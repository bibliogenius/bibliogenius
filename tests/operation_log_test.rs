//! Operation Log Integration Tests
//!
//! Covers: operation_log source column, remote operations, milestones,
//! auto-pinning, log rotation, and echo prevention.

use rust_lib_app::db;
use rust_lib_app::models::operation_log;
use rust_lib_app::sync::{
    log_milestone, log_operation, log_operation_with_str_id, log_remote_operation,
    set_max_operation_log_entries, set_max_pinned_entries,
};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

async fn setup() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

// ── Source column ────────────────────────────────────────────────────

#[tokio::test]
async fn test_local_op_has_source_local() {
    let db = setup().await;
    log_operation(&db, "book", 1, "INSERT", None).await.unwrap();

    let ops = operation_log::Entity::find().all(&db).await.unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].source, "local");
}

#[tokio::test]
async fn test_remote_op_has_device_source() {
    let db = setup().await;
    let id = log_remote_operation(&db, "book", 1, "INSERT", None, 42, false)
        .await
        .unwrap();

    let op = operation_log::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op.source, "device:42");
}

#[tokio::test]
async fn test_remote_op_safety_on_sets_pending_review() {
    let db = setup().await;
    let id = log_remote_operation(&db, "tag", 5, "DELETE", None, 7, true)
        .await
        .unwrap();

    let op = operation_log::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op.status, "pending_review");
    assert_eq!(op.pinned, 0, "Remote ops should never be pinned");
}

#[tokio::test]
async fn test_remote_op_safety_off_sets_pending() {
    let db = setup().await;
    let id = log_remote_operation(&db, "contact", 3, "INSERT", None, 7, false)
        .await
        .unwrap();

    let op = operation_log::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op.status, "pending");
}

// ── Milestones ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_milestone_always_pinned() {
    let db = setup().await;
    log_milestone(
        &db,
        "app_first_launch",
        Some(serde_json::json!({"version": "0.8.0"})),
    )
    .await
    .unwrap();

    let ops = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("MILESTONE"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].pinned, 1);
    assert_eq!(ops[0].operation, "app_first_launch");
    assert_eq!(ops[0].source, "local");
}

// ── Auto-pinning ────────────────────────────────────────────────────

#[tokio::test]
async fn test_first_insert_per_entity_type_is_auto_pinned() {
    let db = setup().await;

    // First book INSERT should be pinned
    log_operation(&db, "book", 1, "INSERT", None).await.unwrap();
    let ops = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("book"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(ops[0].pinned, 1, "First book INSERT should be pinned");

    // Second book INSERT should NOT be pinned
    log_operation(&db, "book", 2, "INSERT", None).await.unwrap();
    let ops = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("book"))
        .order_by_asc(operation_log::Column::Id)
        .all(&db)
        .await
        .unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[1].pinned, 0, "Second book INSERT should NOT be pinned");

    // First contact INSERT should also be pinned (different entity type)
    log_operation(&db, "contact", 1, "INSERT", None)
        .await
        .unwrap();
    let contact_ops = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("contact"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        contact_ops[0].pinned, 1,
        "First contact INSERT should be pinned"
    );
}

#[tokio::test]
async fn test_update_operation_is_never_pinned() {
    let db = setup().await;

    // Even if it's the first operation for this entity type
    log_operation(&db, "book", 1, "UPDATE", None).await.unwrap();
    let ops = operation_log::Entity::find().all(&db).await.unwrap();
    assert_eq!(ops[0].pinned, 0, "UPDATE operations should never be pinned");
}

// ── String ID operations (collections) ──────────────────────────────

#[tokio::test]
async fn test_log_operation_with_str_id_injects_str_id() {
    let db = setup().await;
    let payload = serde_json::json!({"name": "My Collection"});

    log_operation_with_str_id(&db, "collection", "uuid-abc-123", "INSERT", Some(payload))
        .await
        .unwrap();

    let ops = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq("collection"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].entity_id, 0, "String-ID ops use entity_id=0");

    // Payload should contain _str_id
    let payload: serde_json::Value =
        serde_json::from_str(ops[0].payload.as_ref().unwrap()).unwrap();
    assert_eq!(payload["_str_id"], "uuid-abc-123");
    assert_eq!(payload["name"], "My Collection");
}

// ── Log rotation / pruning ──────────────────────────────────────────

#[tokio::test]
async fn test_log_rotation_prunes_old_non_pinned() {
    let db = setup().await;

    // Set very low limits for testing
    set_max_operation_log_entries(5);
    set_max_pinned_entries(10);

    // Insert 200 non-pinned UPDATE ops (UPDATE is never auto-pinned).
    // The prune counter is a global AtomicU32 shared across all tests,
    // firing every 50 inserts. After a prune cycle, up to 49 new entries
    // can accumulate before the next prune.
    let total_inserted: usize = 200;
    for i in 1..=(total_inserted as i32) {
        log_operation(&db, "book", i, "UPDATE", None).await.unwrap();
    }

    let count = operation_log::Entity::find()
        .filter(operation_log::Column::Pinned.eq(0))
        .all(&db)
        .await
        .unwrap()
        .len();

    // After at least one prune cycle, the count should be much less than total_inserted.
    // The prune deletes down to max (5), then up to 49 entries can accumulate
    // before the next prune. So the upper bound is max + PRUNE_INTERVAL - 1 = 54.
    assert!(
        count <= 54,
        "Pruning should keep non-pinned entries bounded, got {count} (inserted {total_inserted})"
    );
    // Also verify pruning actually happened (count is significantly less than total)
    assert!(
        count < total_inserted / 2,
        "Pruning should have removed entries, but {count} of {total_inserted} remain"
    );

    // Reset to defaults for other tests
    set_max_operation_log_entries(500);
}

// ── Echo prevention ─────────────────────────────────────────────────

#[tokio::test]
async fn test_echo_prevention_only_local_ops_returned() {
    let db = setup().await;

    // Insert local + remote ops
    log_operation(&db, "book", 1, "INSERT", None).await.unwrap();
    log_operation(&db, "tag", 1, "INSERT", None).await.unwrap();
    log_remote_operation(&db, "book", 2, "INSERT", None, 99, false)
        .await
        .unwrap();

    // Query only local ops (what DeviceSyncService.get_local_ops_since does)
    let local_ops = operation_log::Entity::find()
        .filter(operation_log::Column::Source.eq("local"))
        .all(&db)
        .await
        .unwrap();

    assert_eq!(local_ops.len(), 2, "Should only return local ops");
    assert!(local_ops.iter().all(|op| op.source == "local"));
}
