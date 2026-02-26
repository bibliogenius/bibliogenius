//! Linked device repository trait and related types

use async_trait::async_trait;

use super::DomainError;

/// A device linked to this node for multi-device sync
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LinkedDevice {
    pub id: Option<i32>,
    pub name: String,
    pub ed25519_public_key: Vec<u8>,
    pub x25519_public_key: Vec<u8>,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
    pub relay_write_token: Option<String>,
    pub last_synced: Option<String>,
    pub created_at: Option<String>,
}

/// Input for registering a new linked device
#[derive(Debug, Clone)]
pub struct CreateLinkedDeviceInput {
    pub name: String,
    pub ed25519_public_key: Vec<u8>,
    pub x25519_public_key: Vec<u8>,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
    pub relay_write_token: Option<String>,
}

/// Repository trait for linked devices
#[async_trait]
pub trait LinkedDeviceRepository: Send + Sync {
    /// List all linked devices
    async fn find_all(&self) -> Result<Vec<LinkedDevice>, DomainError>;

    /// Find a linked device by ID
    async fn find_by_id(&self, id: i32) -> Result<Option<LinkedDevice>, DomainError>;

    /// Register a new linked device
    async fn create(&self, input: CreateLinkedDeviceInput) -> Result<LinkedDevice, DomainError>;

    /// Update the last_synced timestamp for a device
    async fn update_last_synced(&self, id: i32, timestamp: &str) -> Result<(), DomainError>;

    /// Remove a linked device
    async fn delete(&self, id: i32) -> Result<(), DomainError>;
}
