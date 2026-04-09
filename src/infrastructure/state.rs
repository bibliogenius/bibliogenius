//! Application state containing repositories and shared resources

use sea_orm::DatabaseConnection;
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::domain::{
    AuthorRepository, BookRepository, CollectionRepository, CopyRepository, GamificationRepository,
    LinkedDeviceRepository, LoanSettingsRepository, NotificationRepository,
};
use crate::infrastructure::nonce_store::SqliteNonceStore;
use crate::infrastructure::{
    SeaOrmAuthorRepository, SeaOrmBookRepository, SeaOrmCollectionRepository, SeaOrmCopyRepository,
    SeaOrmGamificationRepository, SeaOrmLinkedDeviceRepository, SeaOrmLoanSettingsRepository,
    SeaOrmNotificationRepository,
};
use crate::services::IdentityService;
use crate::services::crypto_service::CryptoService;
use crate::services::device_pairing_service::DevicePairingService;
use crate::services::device_sync_service::DeviceSyncService;
use crate::services::hub_directory_service::HubDirectoryService;

/// Pending relay request-response entry (ADR-012).
/// When a relay request is sent with a `correlation_id`, a oneshot sender is stored here.
/// The relay poller resolves it when a matching response arrives.
type PendingRelayRequests =
    Arc<dashmap::DashMap<String, tokio::sync::oneshot::Sender<serde_json::Value>>>;

/// Cache of peers whose direct LAN connection recently failed.
/// Key = peer id, value = timestamp of the failure.
/// Entries older than [`PEER_UNREACHABLE_TTL`] are treated as expired.
type PeerDirectFailures = Arc<dashmap::DashMap<i32, std::time::Instant>>;

/// Guard against concurrent `poll_once()` executions.
/// Multiple callers (timer, WS nudge, poll_now endpoint, peer.rs) could overlap
/// and fetch the same relay messages before acks go through, causing double processing.
/// `try_lock()` in `poll_once()` skips any cycle that races an already-running one.
type RelayPollLock = Arc<tokio::sync::Mutex<()>>;

/// How long a peer stays in the "unreachable via direct" cache before we retry.
const PEER_UNREACHABLE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Application state shared across all handlers
#[derive(Clone)]
pub struct AppState {
    /// Database connection (for backward compatibility)
    db: DatabaseConnection,
    /// Actual HTTP server port (may differ from 8000 if occupied)
    server_port: Arc<std::sync::atomic::AtomicU16>,
    /// Book repository
    pub book_repo: Arc<dyn BookRepository>,
    /// Author repository
    pub author_repo: Arc<dyn AuthorRepository>,
    /// Copy repository
    pub copy_repo: Arc<dyn CopyRepository>,
    /// Collection repository
    pub collection_repo: Arc<dyn CollectionRepository>,
    /// Gamification repository
    pub gamification_repo: Arc<dyn GamificationRepository>,
    /// Linked device repository (multi-device sync)
    pub linked_device_repo: Arc<dyn LinkedDeviceRepository>,
    /// Notification repository (activity feed)
    pub notification_repo: Arc<dyn NotificationRepository>,
    /// Loan settings repository (loan duration configuration)
    pub loan_settings_repo: Arc<dyn LoanSettingsRepository>,
    /// Identity service for E2EE key management
    pub identity_service: Arc<IdentityService>,
    /// Crypto service for E2EE seal/open (lazily initialized after identity is ready)
    crypto_service: Arc<OnceCell<Arc<CryptoService<SqliteNonceStore>>>>,
    /// Device pairing service for multi-device sync
    pub device_pairing: Arc<DevicePairingService>,
    /// Device sync service for operation log exchange
    pub device_sync: Arc<DeviceSyncService>,
    /// Pending relay request-response correlation map (ADR-012).
    pending_relay_requests: PendingRelayRequests,
    /// Hub directory service — manages public directory and follow relationships (ADR-015).
    pub hub_directory: Arc<HubDirectoryService>,
    /// Cache of peers whose direct LAN connection recently failed.
    /// Used by `try_send_e2ee` to skip direct and go straight to relay.
    peer_direct_failures: PeerDirectFailures,
    /// Relay poll lock — prevents concurrent `poll_once()` from double-processing messages.
    relay_poll_lock: RelayPollLock,
}

impl AppState {
    /// Create a new AppState with all repositories initialized
    pub fn new(db: DatabaseConnection) -> Self {
        let identity_service = Arc::new(IdentityService::new(db.clone()));
        Self::with_identity_service(db, identity_service)
    }

    /// Create AppState with a shared IdentityService (used in FFI mode
    /// so the HTTP server shares the same identity initialized by Flutter).
    pub fn with_identity_service(
        db: DatabaseConnection,
        identity_service: Arc<IdentityService>,
    ) -> Self {
        let book_repo = Arc::new(SeaOrmBookRepository::new(db.clone()));
        let author_repo = Arc::new(SeaOrmAuthorRepository::new(db.clone()));
        let copy_repo = Arc::new(SeaOrmCopyRepository::new(db.clone()));
        let collection_repo = Arc::new(SeaOrmCollectionRepository::new(db.clone()));
        let gamification_repo = Arc::new(SeaOrmGamificationRepository::new(db.clone()));
        let linked_device_repo = Arc::new(SeaOrmLinkedDeviceRepository::new(db.clone()));
        let notification_repo = Arc::new(SeaOrmNotificationRepository::new(db.clone()));
        let loan_settings_repo = Arc::new(SeaOrmLoanSettingsRepository::new(db.clone()));

        // Reuse the FFI-initialized pairing service so code generation
        // and HTTP acceptance share the same in-memory offer store.
        let device_pairing = crate::api::frb::shared_device_pairing_svc()
            .cloned()
            .unwrap_or_else(|| {
                Arc::new(DevicePairingService::new(
                    identity_service.clone(),
                    linked_device_repo.clone(),
                ))
            });

        let device_sync = Arc::new(DeviceSyncService::new(
            db.clone(),
            linked_device_repo.clone(),
        ));

        Self {
            db,
            server_port: Arc::new(std::sync::atomic::AtomicU16::new(8000)),
            book_repo,
            author_repo,
            copy_repo,
            collection_repo,
            gamification_repo,
            linked_device_repo,
            notification_repo,
            loan_settings_repo,
            identity_service,
            crypto_service: Arc::new(OnceCell::new()),
            device_pairing,
            device_sync,
            pending_relay_requests: Arc::new(dashmap::DashMap::new()),
            hub_directory: Arc::new(HubDirectoryService::new()),
            peer_direct_failures: Arc::new(dashmap::DashMap::new()),
            relay_poll_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Get the CryptoService, lazily initializing it from the IdentityService.
    /// Returns None if identity hasn't been initialized yet.
    pub fn crypto_service(&self) -> Option<&Arc<CryptoService<SqliteNonceStore>>> {
        // Try to get already-initialized service
        if let Some(svc) = self.crypto_service.get() {
            return Some(svc);
        }

        // Try to initialize from identity service
        if let Ok(identity) = self.identity_service.identity() {
            let (ed_bytes, x_bytes) = identity.export_secret_bytes();
            let crypto_identity =
                crate::crypto::identity::NodeIdentity::from_bytes(&ed_bytes, &x_bytes);
            let nonce_store = SqliteNonceStore::new(self.db.clone());
            let crypto = CryptoService::new(crypto_identity, nonce_store);
            // set() may fail if another thread raced us — that's fine
            let _ = self.crypto_service.set(Arc::new(crypto));
            tracing::info!("E2EE: CryptoService initialized");
            self.crypto_service.get()
        } else {
            None
        }
    }

    // ── Relay request-response correlation (ADR-012) ───────────────────

    /// Register a pending relay request. Returns a oneshot receiver that will
    /// be resolved when the relay poller receives a matching response.
    pub fn register_relay_request(
        &self,
        correlation_id: String,
    ) -> tokio::sync::oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_relay_requests.insert(correlation_id, tx);
        rx
    }

    /// Try to resolve a pending relay request by correlation_id.
    /// Returns true if a listener was found and the value was sent.
    pub fn resolve_relay_request(&self, correlation_id: &str, value: serde_json::Value) -> bool {
        if let Some((_, tx)) = self.pending_relay_requests.remove(correlation_id) {
            tx.send(value).is_ok()
        } else {
            false
        }
    }

    /// Clean up a pending relay request (e.g. on timeout).
    pub fn cancel_relay_request(&self, correlation_id: &str) {
        self.pending_relay_requests.remove(correlation_id);
    }

    /// Get the actual HTTP server port.
    pub fn server_port(&self) -> u16 {
        self.server_port.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the actual HTTP server port (called after binding).
    pub fn set_server_port(&self, port: u16) {
        self.server_port
            .store(port, std::sync::atomic::Ordering::Relaxed);
    }

    /// Get our public URL using the actual server port.
    pub fn our_public_url(&self) -> String {
        crate::utils::net::get_public_url(self.server_port())
    }

    // ── Peer reachability cache ─────────────────────────────────────

    /// Mark a peer as unreachable via direct LAN connection.
    /// Subsequent calls to `is_peer_direct_unreachable` will return `true`
    /// until [`PEER_UNREACHABLE_TTL`] expires.
    pub fn mark_peer_direct_failed(&self, peer_id: i32) {
        self.peer_direct_failures
            .insert(peer_id, std::time::Instant::now());
    }

    /// Check whether a peer is in the "unreachable via direct" cache.
    /// Returns `true` if the peer failed recently (within TTL).
    /// Expired entries are removed lazily.
    pub fn is_peer_direct_unreachable(&self, peer_id: i32) -> bool {
        if let Some(entry) = self.peer_direct_failures.get(&peer_id) {
            if entry.value().elapsed() < PEER_UNREACHABLE_TTL {
                return true;
            }
            // Expired -- remove lazily
            drop(entry);
            self.peer_direct_failures.remove(&peer_id);
        }
        false
    }

    /// Clear the "unreachable" mark for a peer (e.g. when direct succeeds).
    pub fn clear_peer_direct_failed(&self, peer_id: i32) {
        self.peer_direct_failures.remove(&peer_id);
    }

    // ── Relay poll deduplication ────────────────────────────────────

    /// Return the relay poll lock.
    ///
    /// `poll_once()` uses `try_lock()` on this to ensure only one poll cycle
    /// runs at a time. Callers that race an already-running cycle are silently
    /// skipped — the running poll will process all pending messages.
    pub fn relay_poll_lock(&self) -> &RelayPollLock {
        &self.relay_poll_lock
    }

    /// Get the database connection (for backward compatibility during migration)
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}

// Allow extracting DatabaseConnection from AppState for backward compatibility
impl AsRef<DatabaseConnection> for AppState {
    fn as_ref(&self) -> &DatabaseConnection {
        &self.db
    }
}

// Implement FromRef to allow extracting DatabaseConnection from AppState
impl axum::extract::FromRef<AppState> for DatabaseConnection {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state() -> AppState {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let _ = crate::infrastructure::db::run_migrations(&db).await;
        AppState::new(db)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_unreachable_mark_and_check() {
        let state = test_state().await;
        assert!(!state.is_peer_direct_unreachable(42));

        state.mark_peer_direct_failed(42);
        assert!(state.is_peer_direct_unreachable(42));
        // Other peers unaffected
        assert!(!state.is_peer_direct_unreachable(99));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_unreachable_clear() {
        let state = test_state().await;
        state.mark_peer_direct_failed(42);
        assert!(state.is_peer_direct_unreachable(42));

        state.clear_peer_direct_failed(42);
        assert!(!state.is_peer_direct_unreachable(42));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_unreachable_expiry() {
        let state = test_state().await;
        // Insert a synthetic entry far in the past to simulate expiry
        state
            .peer_direct_failures
            .insert(42, std::time::Instant::now() - PEER_UNREACHABLE_TTL);

        // Should be expired (elapsed >= TTL)
        assert!(!state.is_peer_direct_unreachable(42));
        // Entry should have been lazily removed
        assert!(!state.peer_direct_failures.contains_key(&42));
    }
}
