use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "peers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    /// User-defined display name (overrides `name` in the UI)
    pub display_name: Option<String>,
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
    /// ISO 8601 timestamp set when this peer's `relay_write_token` has been
    /// proven invalid (404 from hub after a failed credential refresh) per
    /// ADR-032. NULL = valid. Cleared when a fresh write_token is persisted
    /// via `refresh_peer_relay_credentials` or accept_connection_request.
    /// Gate short-circuits deposits while this field is Some to stop the
    /// retry flood against a mailbox the hub no longer has.
    pub relay_write_token_invalid_at: Option<String>,
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
    /// JSON avatar configuration from the remote peer's profile
    pub avatar_config: Option<String>,
    /// Last `operation_log.id` we successfully applied from this peer
    /// (ADR-028 delta sync). NULL means no successful sync yet — the next
    /// pull will be a full GET.
    pub last_delta_cursor: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// One hour rate limit on stale-invite retries (ADR-032). After this window,
/// the gate admits a single deposit attempt. Success clears the flag via
/// `refresh_peer_relay_credentials`, failure re-stamps the timestamp. Stops
/// the deposit flood while preserving auto-recovery when the peer returns.
pub const RELAY_WRITE_TOKEN_RETRY_AFTER_SECS: i64 = 3600;

impl Model {
    /// Returns whether a relay deposit toward this peer is currently allowed
    /// under the ADR-032 gate. `true` if the write_token has never been
    /// flagged invalid, or the last invalidation is older than
    /// `RELAY_WRITE_TOKEN_RETRY_AFTER_SECS`.
    pub fn relay_gate_allows_send(&self) -> bool {
        let Some(ts_str) = self.relay_write_token_invalid_at.as_deref() else {
            return true;
        };
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) else {
            // Malformed timestamp shouldn't permanently brick the peer.
            return true;
        };
        let elapsed = chrono::Utc::now()
            .signed_duration_since(ts.with_timezone(&chrono::Utc))
            .num_seconds();
        elapsed >= RELAY_WRITE_TOKEN_RETRY_AFTER_SECS
    }

    /// `true` when the peer is currently in the "invitation stale" state
    /// (flag set, regardless of retry window). Used for UI badge + FFI.
    pub fn relay_write_token_flagged_invalid(&self) -> bool {
        self.relay_write_token_invalid_at.is_some()
    }
}
