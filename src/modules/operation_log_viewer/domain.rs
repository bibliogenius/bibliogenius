//! Domain types for the operation log viewer module

use async_trait::async_trait;
use serde::Serialize;

use crate::domain::DomainError;

/// A single operation log entry for display
#[derive(Debug, Clone, Serialize)]
pub struct OperationLogEntry {
    pub id: i32,
    pub entity_type: String,
    pub entity_id: i32,
    pub operation: String,
    pub payload: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub pinned: bool,
    pub created_at: String,
}

/// Filter for querying operation log entries
#[derive(Debug, Default)]
pub struct OperationLogFilter {
    pub entity_type: Option<String>,
    pub operation: Option<String>,
    pub status: Option<String>,
    pub query: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub page: u64,
    pub limit: u64,
}

/// Paginated response
#[derive(Debug, Serialize)]
pub struct OperationLogPage {
    pub entries: Vec<OperationLogEntry>,
    pub total: u64,
    pub page: u64,
    pub limit: u64,
}

/// Aggregated stats
#[derive(Debug, Serialize)]
pub struct OperationLogStats {
    pub total: u64,
    pub today: u64,
    pub pending: u64,
    pub failed: u64,
}

#[async_trait]
pub trait OperationLogViewerRepository: Send + Sync {
    async fn find_all(&self, filter: OperationLogFilter) -> Result<OperationLogPage, DomainError>;

    async fn get_stats(&self) -> Result<OperationLogStats, DomainError>;

    async fn get_entity_types(&self) -> Result<Vec<String>, DomainError>;
}
