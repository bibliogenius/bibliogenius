//! Device pairing service for multi-device sync (ADR-011).
//!
//! Manages ephemeral pairing codes and device registration flow:
//! 1. Device A generates a 6-digit pairing offer (in-memory, 5-min TTL)
//! 2. Device B enters the code, receives A's crypto keys, and registers as linked
//! 3. Device A receives B's keys and registers B as linked
//!
//! Pairing codes are one-time use. Crypto keys come from IdentityService.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::domain::{CreateLinkedDeviceInput, DomainError, LinkedDevice, LinkedDeviceRepository};
use crate::services::IdentityService;

const PAIRING_CODE_TTL_SECS: i64 = 300; // 5 minutes

/// Brute-force protection for `accept_offer`. A 6-digit code is only 1M
/// combinations, so without a limiter an attacker on the LAN could try every
/// code while an offer is live. We cap failed accepts within a sliding window.
const MAX_FAILED_ACCEPTS: usize = 10;
const FAILED_ACCEPT_WINDOW_SECS: i64 = 60;

/// Both Ed25519 and X25519 public keys are exactly 32 bytes.
const PUBLIC_KEY_LEN: usize = 32;

/// Validate the acceptor-provided public keys before persisting them.
///
/// X25519 keys are validated by length only (every 32-byte value is a valid
/// Montgomery u-coordinate). Ed25519 keys are additionally checked to be a
/// valid curve point so a malformed key cannot be stored and later blow up
/// signature verification on the sync path.
fn validate_public_keys(ed25519: &[u8], x25519: &[u8]) -> Result<(), String> {
    if x25519.len() != PUBLIC_KEY_LEN {
        return Err(format!(
            "Invalid X25519 public key length: expected {PUBLIC_KEY_LEN}, got {}",
            x25519.len()
        ));
    }
    let ed_bytes: [u8; PUBLIC_KEY_LEN] = ed25519.try_into().map_err(|_| {
        format!(
            "Invalid Ed25519 public key length: expected {PUBLIC_KEY_LEN}, got {}",
            ed25519.len()
        )
    })?;
    ed25519_dalek::VerifyingKey::from_bytes(&ed_bytes)
        .map_err(|_| "Invalid Ed25519 public key (not a valid curve point)".to_string())?;
    Ok(())
}

/// A pairing offer waiting for an acceptor
#[derive(Clone)]
struct PairingOffer {
    expires_at: DateTime<Utc>,
    library_uuid: String,
    ed25519_public_key: Vec<u8>,
    x25519_public_key: Vec<u8>,
    relay_url: Option<String>,
    mailbox_id: Option<String>,
}

/// Public response returned when generating a pairing offer
#[derive(Debug, Clone, Serialize)]
pub struct PairingOfferResponse {
    pub code: String,
    pub expires_in: u64,
}

/// Input provided by the device accepting the pairing
#[derive(Debug, Clone, Deserialize)]
pub struct PairingAcceptInput {
    pub code: String,
    pub device_name: String,
    pub ed25519_public_key: Vec<u8>,
    pub x25519_public_key: Vec<u8>,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
    pub relay_write_token: Option<String>,
}

/// Confirmation returned after successful pairing accept
#[derive(Debug, Clone, Serialize)]
pub struct PairingConfirmation {
    pub device_id: i32,
    pub library_uuid: String,
    pub offerer_ed25519: Vec<u8>,
    pub offerer_x25519: Vec<u8>,
    pub offerer_relay_url: Option<String>,
    pub offerer_mailbox_id: Option<String>,
}

/// Service managing device pairing codes and linked device lifecycle
pub struct DevicePairingService {
    offers: Arc<Mutex<HashMap<String, PairingOffer>>>,
    /// Timestamps of recent failed accepts, pruned to the sliding window.
    /// Bounded by `MAX_FAILED_ACCEPTS` once pruning kicks in.
    failed_accepts: Arc<Mutex<Vec<DateTime<Utc>>>>,
    identity_service: Arc<IdentityService>,
    linked_device_repo: Arc<dyn LinkedDeviceRepository>,
}

impl DevicePairingService {
    pub fn new(
        identity_service: Arc<IdentityService>,
        linked_device_repo: Arc<dyn LinkedDeviceRepository>,
    ) -> Self {
        Self {
            offers: Arc::new(Mutex::new(HashMap::new())),
            failed_accepts: Arc::new(Mutex::new(Vec::new())),
            identity_service,
            linked_device_repo,
        }
    }

    /// Reject when too many accepts have failed within the sliding window.
    /// Prunes expired entries as a side effect.
    fn check_not_rate_limited(&self) -> Result<(), String> {
        let cutoff = Utc::now() - chrono::Duration::seconds(FAILED_ACCEPT_WINDOW_SECS);
        let mut attempts = self.failed_accepts.lock().unwrap();
        attempts.retain(|t| *t > cutoff);
        if attempts.len() >= MAX_FAILED_ACCEPTS {
            return Err("Too many pairing attempts, try again later".to_string());
        }
        Ok(())
    }

    fn record_failed_accept(&self) {
        self.failed_accepts.lock().unwrap().push(Utc::now());
    }

    fn clear_failed_accepts(&self) {
        self.failed_accepts.lock().unwrap().clear();
    }

    /// Generate a 6-digit pairing offer.
    ///
    /// The caller provides the library UUID and device name.
    /// Crypto keys are pulled from the IdentityService.
    /// Returns the code and TTL.
    pub fn generate_offer(
        &self,
        _device_name: String,
        library_uuid: String,
        relay_url: Option<String>,
        mailbox_id: Option<String>,
        _relay_write_token: Option<String>,
    ) -> Result<PairingOfferResponse, String> {
        let identity = self
            .identity_service
            .identity()
            .map_err(|e| format!("Identity not ready: {e}"))?;

        let ed25519_pk = identity.verifying_key().as_bytes().to_vec();
        let x25519_pk = identity.x25519_public_key().as_bytes().to_vec();

        let mut rng = rand::thread_rng();
        let code: u32 = rng.gen_range(100_000..999_999);
        let code_str = code.to_string();

        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(PAIRING_CODE_TTL_SECS);

        {
            let mut store = self.offers.lock().unwrap();
            // Cleanup expired offers
            store.retain(|_, o| o.expires_at > now);

            store.insert(
                code_str.clone(),
                PairingOffer {
                    expires_at,
                    library_uuid,
                    ed25519_public_key: ed25519_pk,
                    x25519_public_key: x25519_pk,
                    relay_url,
                    mailbox_id,
                },
            );
        }

        tracing::info!(%expires_at, "Device pairing offer generated (5-min TTL)");

        Ok(PairingOfferResponse {
            code: code_str,
            expires_in: PAIRING_CODE_TTL_SECS as u64,
        })
    }

    /// Accept a pairing offer by code.
    ///
    /// Validates the code, registers the acceptor as a linked device,
    /// consumes the code (one-time use), and returns the offerer's info
    /// so the acceptor can store it on their side.
    pub async fn accept_offer(
        &self,
        input: PairingAcceptInput,
    ) -> Result<PairingConfirmation, String> {
        // Brute-force protection: bail out before consuming an offer if the
        // caller has already failed too many times recently.
        self.check_not_rate_limited()?;

        // Reject malformed keys before touching the offer store. A bad key here
        // would otherwise be persisted and later fail on the E2EE sync path.
        if let Err(e) = validate_public_keys(&input.ed25519_public_key, &input.x25519_public_key) {
            self.record_failed_accept();
            tracing::warn!("Device pairing accept rejected: {e}");
            return Err(e);
        }

        // Peek the offer WITHOUT consuming it. The code is one-time use but must
        // survive a transient failure: if registration below fails or the HTTP
        // response is lost, the acceptor must be able to retry the same code
        // within its TTL instead of being told "Invalid" and forced to restart.
        // The code is removed only after the device is successfully persisted.
        let offer = {
            let store = self.offers.lock().unwrap();
            store.get(&input.code).cloned()
        };

        let offer = match offer {
            Some(o) => o,
            None => {
                self.record_failed_accept();
                tracing::warn!("Device pairing accept failed: invalid (unknown) code");
                return Err("Invalid pairing code".to_string());
            }
        };

        if Utc::now() > offer.expires_at {
            // Drop the dead entry so the store doesn't accumulate expired offers.
            self.offers.lock().unwrap().remove(&input.code);
            self.record_failed_accept();
            tracing::warn!("Device pairing accept failed: code expired");
            return Err("Pairing code expired".to_string());
        }

        // Register the acceptor device in our linked_devices table. On failure
        // the offer is intentionally left in place so the acceptor can retry.
        let device = self
            .linked_device_repo
            .create(CreateLinkedDeviceInput {
                name: input.device_name,
                ed25519_public_key: input.ed25519_public_key,
                x25519_public_key: input.x25519_public_key,
                relay_url: input.relay_url,
                mailbox_id: input.mailbox_id,
                relay_write_token: input.relay_write_token,
            })
            .await
            .map_err(|e| {
                tracing::error!("Device pairing accept: registration failed: {e}");
                format!("Failed to register device: {e}")
            })?;

        // Success: consume the code (one-time use) and clear the brute-force counter.
        self.offers.lock().unwrap().remove(&input.code);
        self.clear_failed_accepts();
        tracing::info!(
            device_id = device.id.unwrap_or(0),
            "Device pairing succeeded"
        );

        Ok(PairingConfirmation {
            device_id: device.id.unwrap_or(0),
            library_uuid: offer.library_uuid,
            offerer_ed25519: offer.ed25519_public_key,
            offerer_x25519: offer.x25519_public_key,
            offerer_relay_url: offer.relay_url,
            offerer_mailbox_id: offer.mailbox_id,
        })
    }

    /// List all linked devices
    pub async fn list_devices(&self) -> Result<Vec<LinkedDevice>, DomainError> {
        self.linked_device_repo.find_all().await
    }

    /// Remove a linked device by ID
    pub async fn remove_device(&self, device_id: i32) -> Result<(), DomainError> {
        self.linked_device_repo.delete(device_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::SeaOrmLinkedDeviceRepository;
    use crate::infrastructure::db::init_db;

    /// A real, valid Ed25519 public key (derived from a fixed seed).
    fn valid_ed25519() -> Vec<u8> {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
            .verifying_key()
            .to_bytes()
            .to_vec()
    }

    async fn setup() -> (Arc<IdentityService>, Arc<dyn LinkedDeviceRepository>) {
        let db = init_db("sqlite::memory:").await.unwrap();
        let id_svc = Arc::new(IdentityService::new(db.clone()));
        id_svc
            .init("test-library-uuid")
            .await
            .expect("identity init failed");
        let repo: Arc<dyn LinkedDeviceRepository> = Arc::new(SeaOrmLinkedDeviceRepository::new(db));
        (id_svc, repo)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generate_offer_returns_6_digit_code() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        let resp = svc
            .generate_offer(
                "My Mac".to_string(),
                "lib-uuid-123".to_string(),
                None,
                None,
                None,
            )
            .unwrap();

        assert_eq!(resp.code.len(), 6);
        assert!(resp.code.parse::<u32>().is_ok());
        assert_eq!(resp.expires_in, 300);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_valid_code_creates_linked_device() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo.clone());

        let offer = svc
            .generate_offer(
                "My Mac".to_string(),
                "lib-uuid-123".to_string(),
                Some("wss://relay.example.com".to_string()),
                None,
                None,
            )
            .unwrap();

        let confirmation = svc
            .accept_offer(PairingAcceptInput {
                code: offer.code,
                device_name: "My iPhone".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await
            .unwrap();

        assert_eq!(confirmation.library_uuid, "lib-uuid-123");
        assert!(!confirmation.offerer_ed25519.is_empty());
        assert!(!confirmation.offerer_x25519.is_empty());
        assert_eq!(
            confirmation.offerer_relay_url,
            Some("wss://relay.example.com".to_string())
        );

        // Verify device was stored
        let devices = repo.find_all().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "My iPhone");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_expired_code_fails() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        // Insert an offer with an already-expired timestamp
        {
            let mut store = svc.offers.lock().unwrap();
            store.insert(
                "111111".to_string(),
                PairingOffer {
                    expires_at: Utc::now() - chrono::Duration::seconds(10),
                    library_uuid: "lib-uuid".to_string(),
                    ed25519_public_key: vec![0; 32],
                    x25519_public_key: vec![0; 32],
                    relay_url: None,
                    mailbox_id: None,
                },
            );
        }

        let result = svc
            .accept_offer(PairingAcceptInput {
                code: "111111".to_string(),
                device_name: "Other".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expired"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_invalid_code_fails() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        let result = svc
            .accept_offer(PairingAcceptInput {
                code: "999999".to_string(),
                device_name: "Other".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_same_code_twice_fails() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        let offer = svc
            .generate_offer("Mac".to_string(), "lib-uuid".to_string(), None, None, None)
            .unwrap();

        let code = offer.code.clone();

        // First accept succeeds
        let r1 = svc
            .accept_offer(PairingAcceptInput {
                code: code.clone(),
                device_name: "iPhone".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;
        assert!(r1.is_ok());

        // Second accept fails (code consumed)
        let r2 = svc
            .accept_offer(PairingAcceptInput {
                code,
                device_name: "iPad".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![4; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;
        assert!(r2.is_err());
        assert!(r2.unwrap_err().contains("Invalid"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_and_remove_devices() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        // Generate and accept to create a device
        let offer = svc
            .generate_offer("Mac".to_string(), "lib-uuid".to_string(), None, None, None)
            .unwrap();

        let confirmation = svc
            .accept_offer(PairingAcceptInput {
                code: offer.code,
                device_name: "iPhone".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await
            .unwrap();

        // List should have 1 device
        let devices = svc.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "iPhone");

        // Remove it
        svc.remove_device(confirmation.device_id).await.unwrap();

        // List should be empty
        let devices = svc.list_devices().await.unwrap();
        assert!(devices.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_rejects_malformed_ed25519_key() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo.clone());

        let offer = svc
            .generate_offer("Mac".to_string(), "lib-uuid".to_string(), None, None, None)
            .unwrap();

        // 31-byte Ed25519 key (wrong length) must be rejected.
        let result = svc
            .accept_offer(PairingAcceptInput {
                code: offer.code,
                device_name: "iPhone".to_string(),
                ed25519_public_key: vec![1; 31],
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Ed25519"));
        // The offer must NOT have been consumed by a rejected accept.
        let devices = repo.find_all().await.unwrap();
        assert!(devices.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_rejects_short_x25519_key() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        let offer = svc
            .generate_offer("Mac".to_string(), "lib-uuid".to_string(), None, None, None)
            .unwrap();

        let result = svc
            .accept_offer(PairingAcceptInput {
                code: offer.code,
                device_name: "iPhone".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 16],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("X25519"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_rate_limit_blocks_brute_force() {
        let (id_svc, repo) = setup().await;
        let svc = DevicePairingService::new(id_svc, repo);

        // Hammer with wrong codes (valid keys, so we reach the code check).
        for _ in 0..MAX_FAILED_ACCEPTS {
            let r = svc
                .accept_offer(PairingAcceptInput {
                    code: "000000".to_string(),
                    device_name: "Attacker".to_string(),
                    ed25519_public_key: valid_ed25519(),
                    x25519_public_key: vec![2; 32],
                    relay_url: None,
                    mailbox_id: None,
                    relay_write_token: None,
                })
                .await;
            assert!(r.is_err());
            assert!(r.unwrap_err().contains("Invalid"));
        }

        // Now even a brand-new valid offer cannot be accepted: locked out.
        let offer = svc
            .generate_offer("Mac".to_string(), "lib-uuid".to_string(), None, None, None)
            .unwrap();
        let blocked = svc
            .accept_offer(PairingAcceptInput {
                code: offer.code,
                device_name: "iPhone".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;
        assert!(blocked.is_err());
        assert!(blocked.unwrap_err().contains("Too many"));
    }

    /// A repository whose `create` always fails, to exercise retry-safety.
    struct FailingLinkedDeviceRepository;

    #[async_trait::async_trait]
    impl LinkedDeviceRepository for FailingLinkedDeviceRepository {
        async fn find_all(&self) -> Result<Vec<LinkedDevice>, DomainError> {
            Ok(vec![])
        }
        async fn find_by_id(&self, _id: i32) -> Result<Option<LinkedDevice>, DomainError> {
            Ok(None)
        }
        async fn create(
            &self,
            _input: CreateLinkedDeviceInput,
        ) -> Result<LinkedDevice, DomainError> {
            Err(DomainError::Database(
                "simulated registration failure".to_string(),
            ))
        }
        async fn update_last_synced(&self, _id: i32, _ts: &str) -> Result<(), DomainError> {
            Ok(())
        }
        async fn delete(&self, _id: i32) -> Result<(), DomainError> {
            Ok(())
        }
    }

    /// Retry-safety: a transient registration failure must NOT consume the code.
    /// The acceptor can retry the same code within its TTL instead of restarting.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_retries_after_registration_failure() {
        let db = init_db("sqlite::memory:").await.unwrap();
        let id_svc = Arc::new(IdentityService::new(db.clone()));
        id_svc
            .init("test-library-uuid")
            .await
            .expect("identity init failed");
        let repo: Arc<dyn LinkedDeviceRepository> = Arc::new(FailingLinkedDeviceRepository);
        let svc = DevicePairingService::new(id_svc, repo);

        let offer = svc
            .generate_offer("Mac".to_string(), "lib-uuid".to_string(), None, None, None)
            .unwrap();
        let code = offer.code.clone();

        let r = svc
            .accept_offer(PairingAcceptInput {
                code: code.clone(),
                device_name: "iPhone".to_string(),
                ed25519_public_key: valid_ed25519(),
                x25519_public_key: vec![2; 32],
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
            })
            .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("Failed to register"));

        // The code must still be present for a retry (not burned by the failure).
        assert!(svc.offers.lock().unwrap().contains_key(&code));
    }
}
