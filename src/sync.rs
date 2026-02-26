use crate::models::operation_log;
use sea_orm::*;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};

/// Maximum number of regular (non-pinned) operation log entries to keep.
/// Configurable at runtime via `set_max_operation_log_entries`.
/// Default: 500 entries.
static MAX_LOG_ENTRIES: AtomicU32 = AtomicU32::new(500);

/// Maximum number of pinned milestone entries.
/// Protects against unbounded growth even for milestones.
static MAX_PINNED_ENTRIES: AtomicU32 = AtomicU32::new(100);

/// Counter to avoid pruning on every single insert.
/// Prune check runs every N inserts (currently every 50).
static INSERT_COUNTER: AtomicU32 = AtomicU32::new(0);
const PRUNE_CHECK_INTERVAL: u32 = 50;

/// Set the maximum number of regular operation log entries to retain.
pub fn set_max_operation_log_entries(max: u32) {
    MAX_LOG_ENTRIES.store(max, Ordering::Relaxed);
}

/// Get the current maximum log entries limit.
pub fn get_max_operation_log_entries() -> u32 {
    MAX_LOG_ENTRIES.load(Ordering::Relaxed)
}

/// Set the maximum number of pinned milestone entries.
pub fn set_max_pinned_entries(max: u32) {
    MAX_PINNED_ENTRIES.store(max, Ordering::Relaxed);
}

pub async fn log_operation(
    db: &DatabaseConnection,
    entity_type: &str,
    entity_id: i32,
    operation: &str,
    payload: Option<Value>,
) -> Result<(), DbErr> {
    // Check if this is a "first" for this entity_type+operation - auto-pin milestones
    let should_pin = is_first_occurrence(db, entity_type, operation).await;

    let log = operation_log::ActiveModel {
        entity_type: Set(entity_type.to_owned()),
        entity_id: Set(entity_id),
        operation: Set(operation.to_owned()),
        payload: Set(payload.map(|v| v.to_string())),
        pinned: Set(i32::from(should_pin)),
        source: Set("local".to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    operation_log::Entity::insert(log).exec(db).await?;

    // Periodically prune old entries to keep the table bounded
    let count = INSERT_COUNTER.fetch_add(1, Ordering::Relaxed);
    if count.is_multiple_of(PRUNE_CHECK_INTERVAL) {
        let _ = prune_old_entries(db).await;
    }

    Ok(())
}

/// Log an operation for entities that use string IDs (e.g. collection UUIDs).
/// Stores entity_id=0 and injects "_str_id" into the payload.
pub async fn log_operation_with_str_id(
    db: &DatabaseConnection,
    entity_type: &str,
    str_id: &str,
    operation: &str,
    payload: Option<Value>,
) -> Result<(), DbErr> {
    let mut merged = payload.unwrap_or(Value::Object(serde_json::Map::new()));
    if let Value::Object(ref mut map) = merged {
        map.insert("_str_id".to_string(), Value::String(str_id.to_string()));
    }

    let should_pin = is_first_occurrence(db, entity_type, operation).await;

    let log = operation_log::ActiveModel {
        entity_type: Set(entity_type.to_owned()),
        entity_id: Set(0),
        operation: Set(operation.to_owned()),
        payload: Set(Some(merged.to_string())),
        pinned: Set(i32::from(should_pin)),
        source: Set("local".to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    operation_log::Entity::insert(log).exec(db).await?;

    let count = INSERT_COUNTER.fetch_add(1, Ordering::Relaxed);
    if count.is_multiple_of(PRUNE_CHECK_INTERVAL) {
        let _ = prune_old_entries(db).await;
    }

    Ok(())
}

/// Log a milestone event (app lifecycle: version change, first launch, etc.).
/// Milestones use entity_type = "MILESTONE" and are always pinned.
/// They survive log rotation and form the app's "history" timeline.
pub async fn log_milestone(
    db: &DatabaseConnection,
    event_name: &str,
    payload: Option<Value>,
) -> Result<(), DbErr> {
    let log = operation_log::ActiveModel {
        entity_type: Set("MILESTONE".to_owned()),
        entity_id: Set(0),
        operation: Set(event_name.to_owned()),
        payload: Set(payload.map(|v| v.to_string())),
        pinned: Set(1),
        source: Set("local".to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    operation_log::Entity::insert(log).exec(db).await?;

    // Still prune periodically (handles pinned overflow too)
    let count = INSERT_COUNTER.fetch_add(1, Ordering::Relaxed);
    if count.is_multiple_of(PRUNE_CHECK_INTERVAL) {
        let _ = prune_old_entries(db).await;
    }

    Ok(())
}

/// Log an operation received from a remote device during sync.
/// Uses "device:<id>" as source for echo prevention.
/// Status is "pending_review" when safety mode is on, "pending" when off.
/// No auto-pinning or pruning for remote ops.
/// Returns the inserted operation log entry ID.
pub async fn log_remote_operation(
    db: &DatabaseConnection,
    entity_type: &str,
    entity_id: i32,
    operation: &str,
    payload: Option<Value>,
    source_device_id: i32,
    safety_mode: bool,
) -> Result<i32, DbErr> {
    let status = if safety_mode {
        "pending_review"
    } else {
        "pending"
    };

    let log = operation_log::ActiveModel {
        entity_type: Set(entity_type.to_owned()),
        entity_id: Set(entity_id),
        operation: Set(operation.to_owned()),
        payload: Set(payload.map(|v| v.to_string())),
        pinned: Set(0),
        source: Set(format!("device:{source_device_id}")),
        status: Set(status.to_owned()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    let result = operation_log::Entity::insert(log).exec(db).await?;
    Ok(result.last_insert_id)
}

/// Check if this is the first INSERT for the given entity_type.
/// Only pins the very first INSERT per entity type (not updates or deletes).
async fn is_first_occurrence(db: &DatabaseConnection, entity_type: &str, operation: &str) -> bool {
    if operation != "INSERT" {
        return false;
    }

    // Only auto-pin for syncable entity types (not MILESTONE - those are always pinned)
    let pinnable = matches!(
        entity_type,
        "book" | "contact" | "loan" | "collection" | "copy" | "author" | "tag"
    );
    if !pinnable {
        return false;
    }

    // Check if any INSERT already exists for this entity_type
    let count = operation_log::Entity::find()
        .filter(operation_log::Column::EntityType.eq(entity_type))
        .filter(operation_log::Column::Operation.eq("INSERT"))
        .count(db)
        .await
        .unwrap_or(1); // On error, assume it exists (don't pin)

    // This is the first if count == 0 (the current row hasn't been inserted yet)
    count == 0
}

/// Delete oldest non-pinned entries when the table exceeds the configured maximum.
/// Pinned entries are never pruned (up to MAX_PINNED_ENTRIES).
/// If pinned entries exceed their cap, oldest pinned entries are pruned too.
async fn prune_old_entries(db: &DatabaseConnection) -> Result<(), DbErr> {
    let max = MAX_LOG_ENTRIES.load(Ordering::Relaxed) as u64;
    let max_pinned = MAX_PINNED_ENTRIES.load(Ordering::Relaxed) as u64;

    // Prune non-pinned entries
    let non_pinned_count = operation_log::Entity::find()
        .filter(operation_log::Column::Pinned.eq(0))
        .count(db)
        .await?;

    if non_pinned_count > max {
        let to_delete = non_pinned_count - max;
        let oldest = operation_log::Entity::find()
            .filter(operation_log::Column::Pinned.eq(0))
            .order_by_asc(operation_log::Column::Id)
            .limit(to_delete)
            .all(db)
            .await?;

        if !oldest.is_empty() {
            let ids: Vec<i32> = oldest.iter().map(|e| e.id).collect();
            operation_log::Entity::delete_many()
                .filter(operation_log::Column::Id.is_in(ids))
                .exec(db)
                .await?;
        }
    }

    // Safety cap on pinned entries (should rarely trigger)
    let pinned_count = operation_log::Entity::find()
        .filter(operation_log::Column::Pinned.eq(1))
        .count(db)
        .await?;

    if pinned_count > max_pinned {
        let to_delete = pinned_count - max_pinned;
        let oldest_pinned = operation_log::Entity::find()
            .filter(operation_log::Column::Pinned.eq(1))
            .order_by_asc(operation_log::Column::Id)
            .limit(to_delete)
            .all(db)
            .await?;

        if !oldest_pinned.is_empty() {
            let ids: Vec<i32> = oldest_pinned.iter().map(|e| e.id).collect();
            operation_log::Entity::delete_many()
                .filter(operation_log::Column::Id.is_in(ids))
                .exec(db)
                .await?;
        }
    }

    Ok(())
}

pub mod processor;
