// Operation log inspection over FFI.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Operation Log Viewer FFI ──────────────────────────────────────────

#[frb(dart_metadata=("freezed"))]
pub struct FrbOperationLogEntry {
    pub id: i32,
    pub entity_type: String,
    pub entity_id: String,
    pub operation: String,
    pub payload: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub pinned: bool,
    pub created_at: String,
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbOperationLogStats {
    pub total: u64,
    pub today: u64,
    pub pending: u64,
    pub failed: u64,
}

/// List operation log entries with optional filters
pub async fn operation_log_list(
    entity_type: Option<String>,
    operation: Option<String>,
    status: Option<String>,
    query: Option<String>,
    page: Option<u64>,
    limit: Option<u64>,
) -> Result<Vec<FrbOperationLogEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::operation_log_viewer::domain::{
        OperationLogFilter, OperationLogViewerRepository,
    };
    use crate::modules::operation_log_viewer::repository::SeaOrmOperationLogViewerRepository;

    let repo = SeaOrmOperationLogViewerRepository::new(db);
    let filter = OperationLogFilter {
        entity_type,
        operation,
        status,
        query,
        since: None,
        until: None,
        page: page.unwrap_or(0),
        limit: limit.unwrap_or(50).min(200),
    };

    let page = repo.find_all(filter).await.map_err(|e| e.to_string())?;
    Ok(page
        .entries
        .into_iter()
        .map(|e| FrbOperationLogEntry {
            id: e.id,
            entity_type: e.entity_type,
            entity_id: e.entity_id,
            operation: e.operation,
            payload: e.payload,
            status: e.status,
            error_message: e.error_message,
            pinned: e.pinned,
            created_at: e.created_at,
        })
        .collect())
}

/// Get operation log stats
pub async fn operation_log_stats() -> Result<FrbOperationLogStats, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::operation_log_viewer::domain::OperationLogViewerRepository;
    use crate::modules::operation_log_viewer::repository::SeaOrmOperationLogViewerRepository;

    let repo = SeaOrmOperationLogViewerRepository::new(db);
    let stats = repo.get_stats().await.map_err(|e| e.to_string())?;
    Ok(FrbOperationLogStats {
        total: stats.total,
        today: stats.today,
        pending: stats.pending,
        failed: stats.failed,
    })
}

/// Get distinct entity types for filter dropdowns
pub async fn operation_log_entity_types() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::operation_log_viewer::domain::OperationLogViewerRepository;
    use crate::modules::operation_log_viewer::repository::SeaOrmOperationLogViewerRepository;

    let repo = SeaOrmOperationLogViewerRepository::new(db);
    repo.get_entity_types().await.map_err(|e| e.to_string())
}
