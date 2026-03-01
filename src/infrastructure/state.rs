//! Application state containing repositories and shared resources

use sea_orm::DatabaseConnection;
use std::sync::Arc;
use tokio::sync::OnceCell;

use crate::domain::{
    AuthorRepository, BookRepository, CollectionRepository, CopyRepository, GamificationRepository,
    LinkedDeviceRepository,
};
use crate::infrastructure::nonce_store::SqliteNonceStore;
use crate::infrastructure::{
    SeaOrmAuthorRepository, SeaOrmBookRepository, SeaOrmCollectionRepository, SeaOrmCopyRepository,
    SeaOrmGamificationRepository, SeaOrmLinkedDeviceRepository,
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

/// Application state shared across all handlers
#[derive(Clone)]
pub struct AppState {
    /// Database connection (for backward compatibility)
    db: DatabaseConnection,
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

        let device_pairing = Arc::new(DevicePairingService::new(
            identity_service.clone(),
            linked_device_repo.clone(),
        ));

        let device_sync = Arc::new(DeviceSyncService::new(
            db.clone(),
            linked_device_repo.clone(),
        ));

        Self {
            db,
            book_repo,
            author_repo,
            copy_repo,
            collection_repo,
            gamification_repo,
            linked_device_repo,
            identity_service,
            crypto_service: Arc::new(OnceCell::new()),
            device_pairing,
            device_sync,
            pending_relay_requests: Arc::new(dashmap::DashMap::new()),
            hub_directory: Arc::new(HubDirectoryService::new()),
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
