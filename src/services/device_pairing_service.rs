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

/// A pairing offer waiting for an acceptor
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
            identity_service,
            linked_device_repo,
        }
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
        let offer = {
            let mut store = self.offers.lock().unwrap();
            store.remove(&input.code)
        };

        let offer = offer.ok_or_else(|| "Invalid pairing code".to_string())?;

        if Utc::now() > offer.expires_at {
            return Err("Pairing code expired".to_string());
        }

        // Register the acceptor device in our linked_devices table
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
            .map_err(|e| format!("Failed to register device: {e}"))?;

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
                ed25519_public_key: vec![1; 32],
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
                ed25519_public_key: vec![1; 32],
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
                ed25519_public_key: vec![1; 32],
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
                ed25519_public_key: vec![1; 32],
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
                ed25519_public_key: vec![3; 32],
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
                ed25519_public_key: vec![1; 32],
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
}
