//! Signed device registry for account sync (ADR-042 section 13.5, ADR-043 H3).
//!
//! The set of devices authorized on an account, signed by the account's Ed25519 key
//! (`account_auth_sk`). The hub stores and serves it as an **opaque blob** it never
//! parses (H3); authorization is enforced entirely **client-side**: a client ignores
//! any pulled lane whose `device_id` is absent from the verified registry. Because it
//! is signed, a malicious hub cannot forge a device, inject a lane, or serve divergent
//! views — it can at most withhold or replay an older registry, which the monotonic
//! `registry_seq` lets a client detect.
//!
//! Wire blob = MessagePack(`SignedRegistry { payload, signature }`), where `payload`
//! is the MessagePack of the [`DeviceRegistry`] and the Ed25519 signature is computed
//! over those exact `payload` bytes (so verification never depends on re-serialization
//! being byte-identical).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::errors::CryptoError;

/// One authorized device. The public keys are included so an already-authorized device
/// can seal the trousseau to a new device (enrollment path B) from the registry alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// 256-bit random lane key, base64url (ADR-042 section 13.5).
    pub device_id: String,
    /// Ed25519 public key (device identity, ADR-039).
    pub ed25519_pk: [u8; 32],
    /// X25519 public key (for the sealed path-B transfer).
    pub x25519_pk: [u8; 32],
    /// Human label (e.g. "Federico's iPhone"). Opaque to the hub (inside the blob).
    pub name: String,
}

/// The authorized-device set for one account, serialized + signed + published opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRegistry {
    /// The account this registry belongs to. [`DeviceRegistry::adopt`] checks it
    /// matches the local account (anti cross-account replay); `verify` does NOT.
    pub account_id: String,
    /// Monotonic version, bumped on every publish. [`DeviceRegistry::adopt`] rejects a
    /// registry older than the last one adopted (anti-rollback); `verify` does NOT.
    pub registry_seq: u64,
    pub devices: Vec<DeviceEntry>,
}

impl DeviceRegistry {
    /// Whether `device_id` is an authorized device (the H3 check).
    pub fn is_authorized(&self, device_id: &str) -> bool {
        self.devices.iter().any(|d| d.device_id == device_id)
    }

    /// Look up a device's entry (e.g. to fetch its X25519 key for path-B sealing).
    pub fn device(&self, device_id: &str) -> Option<&DeviceEntry> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }

    /// Return a new registry with `entry` added (or replaced, if its `device_id` is
    /// already present) and `registry_seq` bumped by one — the form an authorized device
    /// signs and publishes to enroll another device. Because the seq advances, a receiver
    /// adopts it as newer (never a rollback). The result is unsigned; call [`Self::sign`].
    pub fn with_device(&self, entry: DeviceEntry) -> DeviceRegistry {
        let mut devices: Vec<DeviceEntry> = self
            .devices
            .iter()
            .filter(|d| d.device_id != entry.device_id)
            .cloned()
            .collect();
        devices.push(entry);
        DeviceRegistry {
            account_id: self.account_id.clone(),
            registry_seq: self.registry_seq + 1,
            devices,
        }
    }

    /// Return a new registry with `device_id` removed and `registry_seq` bumped by one —
    /// the form an authorized device signs and publishes to remove another device from
    /// the account. Because the seq advances, every honest peer adopts it as newer and
    /// stops applying the removed device's lanes (the H3 filter), so the device can no
    /// longer write into the shared library. Removing a `device_id` that is absent is a
    /// no-op on the set but still bumps the seq (idempotent). The result is unsigned;
    /// call [`Self::sign`].
    ///
    /// This is a **soft** removal, not a cryptographic lockout: the removed device still
    /// holds the full trousseau (including the account signing key), so it can still
    /// *read* content encrypted under the current keys and could re-sign a higher-seq
    /// registry re-adding itself. `adopt`'s anti-rollback stops a malicious *hub* from
    /// replaying an old registry, not a *device holder*. Closing that gap requires
    /// account key rotation (a separate concern; see ADR-042 section 13.5).
    ///
    /// The primitive is deliberately **policy-free** (no self / last-device / "who may
    /// remove" guard) so a future authorization model can restrict the caller without
    /// changing this method — the same `without_device` primitive serves both models.
    /// Those guards live at the call sites (the FFI refuses removing the current device).
    pub fn without_device(&self, device_id: &str) -> DeviceRegistry {
        let devices: Vec<DeviceEntry> = self
            .devices
            .iter()
            .filter(|d| d.device_id != device_id)
            .cloned()
            .collect();
        DeviceRegistry {
            account_id: self.account_id.clone(),
            registry_seq: self.registry_seq + 1,
            devices,
        }
    }

    /// Serialize and sign with the account key. Returns the opaque blob to publish.
    pub fn sign(&self, signing_key: &SigningKey) -> Result<Vec<u8>, CryptoError> {
        let payload =
            rmp_serde::to_vec(self).map_err(|e| CryptoError::Serialization(e.to_string()))?;
        let signature = signing_key.sign(&payload);
        let signed = SignedRegistry {
            payload,
            signature: signature.to_bytes().to_vec(),
        };
        rmp_serde::to_vec(&signed).map_err(|e| CryptoError::Serialization(e.to_string()))
    }

    /// Verify an opaque blob against the account public key, returning the registry.
    /// Any tampering (payload or signature) or a wrong key yields `InvalidSignature`.
    pub fn verify(
        blob: &[u8],
        verifying_key: &VerifyingKey,
    ) -> Result<DeviceRegistry, CryptoError> {
        let signed: SignedRegistry =
            rmp_serde::from_slice(blob).map_err(|e| CryptoError::Serialization(e.to_string()))?;
        let signature =
            Signature::from_slice(&signed.signature).map_err(|_| CryptoError::InvalidSignature)?;
        verifying_key
            .verify(&signed.payload, &signature)
            .map_err(|_| CryptoError::InvalidSignature)?;
        rmp_serde::from_slice(&signed.payload)
            .map_err(|e| CryptoError::Serialization(e.to_string()))
    }

    /// Verify AND apply the adoption policy — the method clients use when ingesting a
    /// registry fetched from the hub. On top of [`Self::verify`] (signature only) it
    /// enforces:
    /// - `account_id` matches the local account (anti cross-account replay), and
    /// - `registry_seq` is not older than `last_seen_seq` (anti-rollback).
    ///
    /// `verify` alone is insufficient: a malicious hub can replay an *old, validly
    /// signed* registry to resurrect a revoked device. The caller persists the adopted
    /// `registry_seq` and passes it back as `last_seen_seq` on the next adoption.
    /// Re-adopting the current registry (`seq == last_seen_seq`) is allowed (idempotent).
    pub fn adopt(
        blob: &[u8],
        verifying_key: &VerifyingKey,
        expected_account_id: &str,
        last_seen_seq: u64,
    ) -> Result<DeviceRegistry, RegistryError> {
        let reg = Self::verify(blob, verifying_key)?;
        if reg.account_id != expected_account_id {
            return Err(RegistryError::AccountMismatch);
        }
        if reg.registry_seq < last_seen_seq {
            return Err(RegistryError::Rollback {
                got: reg.registry_seq,
                last_seen: last_seen_seq,
            });
        }
        Ok(reg)
    }
}

/// Failure modes when adopting a fetched registry ([`DeviceRegistry::adopt`]).
#[derive(Debug)]
pub enum RegistryError {
    /// Signature verification or decoding failed.
    Invalid(CryptoError),
    /// The registry is signed for a different account than the local one.
    AccountMismatch,
    /// The registry is older than the last one adopted (a rollback / replay attempt).
    Rollback { got: u64, last_seen: u64 },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(e) => write!(f, "invalid registry: {e}"),
            Self::AccountMismatch => write!(f, "registry signed for a different account"),
            Self::Rollback { got, last_seen } => {
                write!(
                    f,
                    "registry rollback: got seq {got}, last adopted {last_seen}"
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<CryptoError> for RegistryError {
    fn from(e: CryptoError) -> Self {
        Self::Invalid(e)
    }
}

/// Wire wrapper: the registry payload bytes + the detached Ed25519 signature over them.
#[derive(Serialize, Deserialize)]
struct SignedRegistry {
    payload: Vec<u8>,
    signature: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::account_keys::AccountKeyBundle;

    fn entry(id: &str) -> DeviceEntry {
        DeviceEntry {
            device_id: id.to_string(),
            ed25519_pk: [1u8; 32],
            x25519_pk: [2u8; 32],
            name: format!("device {id}"),
        }
    }

    fn registry() -> DeviceRegistry {
        DeviceRegistry {
            account_id: "acct-1".to_string(),
            registry_seq: 3,
            devices: vec![entry("devA"), entry("devB")],
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let bundle = AccountKeyBundle::generate();
        let reg = registry();
        let blob = reg.sign(&bundle.signing_key()).unwrap();
        let restored = DeviceRegistry::verify(&blob, &bundle.verifying_key()).unwrap();
        assert_eq!(restored, reg);
        assert_eq!(restored.registry_seq, 3);
    }

    #[test]
    fn authorization_check() {
        let reg = registry();
        assert!(reg.is_authorized("devA"));
        assert!(reg.is_authorized("devB"));
        assert!(!reg.is_authorized("devX"));
        assert_eq!(reg.device("devA").unwrap().name, "device devA");
        assert!(reg.device("devX").is_none());
    }

    #[test]
    fn with_device_appends_and_bumps_seq() {
        let reg = registry(); // seq 3, devA + devB
        let extended = reg.with_device(entry("devC"));
        assert_eq!(extended.registry_seq, 4);
        assert!(extended.is_authorized("devC"));
        assert_eq!(extended.devices.len(), 3);
        // The source registry is untouched (returns a new value).
        assert_eq!(reg.devices.len(), 2);
    }

    #[test]
    fn with_device_replaces_existing_id_without_duplicating() {
        let reg = registry(); // devA has name "device devA"
        let mut renamed = entry("devA");
        renamed.name = "renamed".to_string();
        let updated = reg.with_device(renamed);
        assert_eq!(updated.devices.len(), 2, "devA must not be duplicated");
        assert_eq!(updated.device("devA").unwrap().name, "renamed");
        assert_eq!(updated.registry_seq, 4);
    }

    #[test]
    fn without_device_removes_entry_and_bumps_seq() {
        let reg = registry(); // seq 3, devA + devB
        let reduced = reg.without_device("devA");
        assert_eq!(reduced.registry_seq, 4);
        assert!(!reduced.is_authorized("devA"));
        assert!(reduced.is_authorized("devB"));
        assert_eq!(reduced.devices.len(), 1);
        // The source registry is untouched (returns a new value).
        assert_eq!(reg.devices.len(), 2);
    }

    #[test]
    fn without_device_absent_id_is_a_seq_bump_noop() {
        let reg = registry(); // seq 3, devA + devB
        let reduced = reg.without_device("devX");
        // Removing an unknown device leaves the set but still advances the seq, so the
        // publish is never seen as a rollback and stays idempotent.
        assert_eq!(reduced.registry_seq, 4);
        assert_eq!(reduced.devices.len(), 2);
        assert!(reduced.is_authorized("devA") && reduced.is_authorized("devB"));
    }

    #[test]
    fn wrong_account_key_rejected() {
        let signer = AccountKeyBundle::generate();
        let attacker = AccountKeyBundle::generate();
        let blob = registry().sign(&signer.signing_key()).unwrap();
        // A different account key (e.g. a malicious hub's) must not verify.
        assert!(DeviceRegistry::verify(&blob, &attacker.verifying_key()).is_err());
    }

    #[test]
    fn tampered_payload_rejected() {
        let bundle = AccountKeyBundle::generate();
        let mut blob = registry().sign(&bundle.signing_key()).unwrap();
        // Flip a byte; either the signature check or the decode fails, never silently OK.
        let mid = blob.len() / 2;
        blob[mid] ^= 0xFF;
        assert!(DeviceRegistry::verify(&blob, &bundle.verifying_key()).is_err());
    }

    #[test]
    fn adopt_accepts_newer_or_equal_seq() {
        let bundle = AccountKeyBundle::generate();
        let blob = registry().sign(&bundle.signing_key()).unwrap(); // seq = 3
        // Newer than last seen.
        assert!(DeviceRegistry::adopt(&blob, &bundle.verifying_key(), "acct-1", 2).is_ok());
        // Idempotent re-adoption of the current registry.
        assert!(DeviceRegistry::adopt(&blob, &bundle.verifying_key(), "acct-1", 3).is_ok());
    }

    #[test]
    fn adopt_rejects_rollback() {
        let bundle = AccountKeyBundle::generate();
        let blob = registry().sign(&bundle.signing_key()).unwrap(); // seq = 3
        // A hub replays this seq-3 registry after we already adopted seq 5.
        let err = DeviceRegistry::adopt(&blob, &bundle.verifying_key(), "acct-1", 5).unwrap_err();
        assert!(matches!(
            err,
            RegistryError::Rollback {
                got: 3,
                last_seen: 5
            }
        ));
    }

    #[test]
    fn adopt_rejects_account_mismatch() {
        let bundle = AccountKeyBundle::generate();
        let blob = registry().sign(&bundle.signing_key()).unwrap(); // account_id = acct-1
        let err =
            DeviceRegistry::adopt(&blob, &bundle.verifying_key(), "other-acct", 0).unwrap_err();
        assert!(matches!(err, RegistryError::AccountMismatch));
    }

    #[test]
    fn adopt_rejects_bad_signature() {
        let signer = AccountKeyBundle::generate();
        let attacker = AccountKeyBundle::generate();
        let blob = registry().sign(&signer.signing_key()).unwrap();
        let err = DeviceRegistry::adopt(&blob, &attacker.verifying_key(), "acct-1", 0).unwrap_err();
        assert!(matches!(err, RegistryError::Invalid(_)));
    }
}
