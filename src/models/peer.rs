use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "peers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    #[sea_orm(unique)]
    pub url: String,
    /// Stable UUID for P2P deduplication (survives IP changes)
    pub library_uuid: Option<String>,
    pub public_key: Option<String>,
    /// Hex-encoded X25519 public key for E2EE key exchange
    pub x25519_public_key: Option<String>,
    /// Whether both Ed25519 and X25519 keys have been exchanged with this peer
    #[sea_orm(default_value = "0")]
    pub key_exchange_done: bool,
    /// Mailbox UUID for offline message relay via hub
    pub mailbox_id: Option<String>,
    /// Relay hub URL for this peer (e.g., https://hub.bibliogenius.org)
    pub relay_url: Option<String>,
    /// Write token for depositing messages in this peer's relay mailbox
    pub relay_write_token: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    #[sea_orm(default_value = "false")]
    pub auto_approve: bool,
    /// Connection status: "pending" or "accepted"
    #[sea_orm(default_value = "accepted")]
    pub connection_status: String,
    pub last_seen: Option<String>,
    /// SHA-256 hash of peer's catalog for change detection (ADR-012)
    pub catalog_hash: Option<String>,
    /// ISO 8601 timestamp of last catalog sync (ADR-012)
    pub last_catalog_sync: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
