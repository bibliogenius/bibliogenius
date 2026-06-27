//! Account-level E2EE key hierarchy for multi-device sync (ADR-042,
//! `SECURITY_GUIDELINES.md` PARTIE F).
//!
//! This is a SEPARATE layer from the peer-to-peer transport crypto in
//! [`super::envelope`] / [`crate::services::crypto_service`]. The transport layer
//! secures device-to-device messages with ephemeral-DH sign-then-encrypt; this
//! layer secures entity blobs **at rest** in the hub's blind lane store (ADR-043),
//! keyed by opaque identifiers the hub cannot invert.
//!
//! Pipeline (ADR-042 §2):
//! ```text
//! passphrase --Argon2id(p=1)--> MK --HKDF--> KWK            (wraps the bundle)
//!                                  \--HKDF--> AuthVerifier   (gates bundle download)
//!
//! AccountKeyBundle { adk, aik, account_auth_sk }   random (OsRng), in-RAM only,
//!                                                  wrapped by KWK (and by RWK for recovery)
//!
//! per entity:
//!   opaque_id = HMAC-SHA256(AIK, entity_type || 0x1F || entity_uuid)
//!   CEK       = HKDF-SHA256(ikm=ADK, salt=opaque_id, info="bg-acct-v1|entity-content")
//!   blob      = AES-256-GCM(CEK, nonce=OsRng96,
//!                           aad = "bg-acct-v1|blob" || account_id || opaque_id || device_id,
//!                           plaintext = pad_bucket(changeset))
//! ```
//!
//! v1 uses AES-256-GCM (not GCM-SIV): the per-entity CEK plus a fresh random nonce
//! make plain GCM unconditionally safe (ADR-042 §11.1). [`AEAD_ALG_V1`] is recorded
//! on the account so a future upgrade to GCM-SIV is an authenticated, non-downgradable
//! negotiation (ADR-042 §14/M5).
//!
//! All key material is held in [`zeroize`] wrappers and the bundle never derives
//! `Debug`/`Display`, per `SECURITY_GUIDELINES.md` A1/A2.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::StaticSecret;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::encryption::generate_nonce;
use super::errors::CryptoError;
use super::sealed_blob;

/// AEAD algorithm identifier recorded on the account (ADR-042 §14/M5). Clients pin
/// this value and refuse a downgrade; v1 ships plain AES-256-GCM.
pub const AEAD_ALG_V1: &str = "AES-256-GCM";

/// Account schema version, pinned and authenticated to refuse downgrade (M5).
pub const ACCOUNT_SCHEMA_VERSION: u32 = 1;

// --- HKDF domain-separation labels (M5: IKM=secret, salt per context, label in info) ---
// L3 nit: passphrase and recovery wrapping keys use DISTINCT labels.
const HKDF_INFO_KWK: &[u8] = b"bg-acct-v1|key-wrap-passphrase";
const HKDF_INFO_RWK: &[u8] = b"bg-acct-v1|key-wrap-recovery";
const HKDF_INFO_AUTH_VERIFIER: &[u8] = b"bg-acct-v1|bundle-fetch";
const HKDF_INFO_ENTITY_CONTENT: &[u8] = b"bg-acct-v1|entity-content";

// --- AEAD additional-authenticated-data prefixes ---
const BLOB_AAD_PREFIX: &[u8] = b"bg-acct-v1|blob";
const BUNDLE_AAD_PASSPHRASE: &[u8] = b"bg-acct-v1|bundle-passphrase";
const BUNDLE_AAD_RECOVERY: &[u8] = b"bg-acct-v1|bundle-recovery";
/// AAD for the device-local at-rest wrap of the trousseau (ADR-042 §14 client
/// persistence addendum). Distinct from the passphrase/recovery wrap and the device
/// seal, so an at-rest blob can never be mistaken for any other wrapped copy.
const BUNDLE_AAD_AT_REST: &[u8] = b"bg-acct-v1|bundle-at-rest";

/// Separator between entity type and uuid in the opaque-id HMAC input (ADR-042 §6).
const OPAQUE_ID_SEP: u8 = 0x1F;

/// Domain prefix for the canonical account descriptor that is signed at signup (ADR-042 §3).
const DESCRIPTOR_DOMAIN: &[u8] = b"bg-acct-v1|descriptor";

// Argon2id ACCOUNT profile (ADR-042 §3, §14/H4). DISTINCT from the device-local
// profile in `encryption::derive_key_from_password` (which uses p=4): the account
// KDF uses p=1 for WASM single-thread parity with the future web client.
const ACCT_ARGON2_M_COST: u32 = 65536; // 64 MiB
const ACCT_ARGON2_T_COST: u32 = 3;
const ACCT_ARGON2_P_COST: u32 = 1;

// Padding buckets for entity plaintext (ADR-042 §6). The floor is raised to 1 KiB
// per §14/M1 so small entity types are folded together and harder to fingerprint.
const PAD_BUCKETS: &[usize] = &[1024, 2048, 4096, 8192, 16384];
const PAD_STEP_ABOVE: usize = 16384;

/// Serialized length of an [`AccountKeyBundle`]: adk(32) + aik(32) + auth seed(32).
const BUNDLE_PLAINTEXT_LEN: usize = 96;

/// Argon2id parameters for account key derivation. Stored with the account (public,
/// not secret) and downloaded with the salt so every device and the web client derive
/// an identical Master Key (ADR-042 §3 — never hard-coded on the read path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Argon2Params {
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for Argon2Params {
    /// The single mandatory v1 profile (ADR-042 §3, §14/H4: the 19 MiB fallback is removed).
    fn default() -> Self {
        Self {
            m_cost: ACCT_ARGON2_M_COST,
            t_cost: ACCT_ARGON2_T_COST,
            p_cost: ACCT_ARGON2_P_COST,
        }
    }
}

/// Which wrapping key a wrapped bundle is bound to (drives the AEAD AAD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapKind {
    /// Wrapped under KWK derived from the passphrase Master Key.
    Passphrase,
    /// Wrapped under RWK derived from the recovery key (ADR-042 §8).
    Recovery,
}

impl WrapKind {
    fn aad(self) -> &'static [u8] {
        match self {
            Self::Passphrase => BUNDLE_AAD_PASSPHRASE,
            Self::Recovery => BUNDLE_AAD_RECOVERY,
        }
    }

    /// The on-the-wire `kind` label the hub stores for this wrapped copy
    /// (matches `WrappedAccountKey::KIND_*` on the hub).
    pub fn wire_kind(self) -> &'static str {
        match self {
            Self::Passphrase => "passphrase",
            Self::Recovery => "recovery",
        }
    }
}

/// The account trousseau: random keys shared by all devices of an account, decrypted
/// only in device RAM. Never persisted in the clear, never logged (no `Debug`).
///
/// Fields are private so callers go through the seal/open/wrap helpers and cannot
/// copy raw key bytes out by accident.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct AccountKeyBundle {
    /// Account Data Key — root key for per-entity content encryption.
    adk: [u8; 32],
    /// Account Index Key — HMAC key for opaque entity identifiers.
    aik: [u8; 32],
    /// Ed25519 secret seed for account-to-hub authentication (challenge-response).
    account_auth_seed: [u8; 32],
}

impl AccountKeyBundle {
    /// Generate a fresh trousseau with cryptographically random keys (ADR-042 §2:
    /// ADK/AIK are random, never derived from the passphrase).
    pub fn generate() -> Self {
        let mut adk = [0u8; 32];
        let mut aik = [0u8; 32];
        let mut account_auth_seed = [0u8; 32];
        OsRng.fill_bytes(&mut adk);
        OsRng.fill_bytes(&mut aik);
        OsRng.fill_bytes(&mut account_auth_seed);
        Self {
            adk,
            aik,
            account_auth_seed,
        }
    }

    /// The Ed25519 signing key used to authenticate the account to the hub.
    pub fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.account_auth_seed)
    }

    /// The Ed25519 verifying (public) key registered with the hub at signup.
    pub fn account_auth_pk(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }

    /// The Ed25519 verifying key.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key().verifying_key()
    }

    /// Opaque hub identifier for an entity (ADR-042 §6): `HMAC-SHA256(AIK, type || 0x1F || uuid)`.
    /// The hub cannot invert it, link it to a real uuid, or tell entity types apart.
    pub fn opaque_id(&self, entity_type: &str, entity_uuid: &str) -> [u8; 32] {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.aik)
            .expect("HMAC accepts keys of any length");
        mac.update(entity_type.as_bytes());
        mac.update(&[OPAQUE_ID_SEP]);
        mac.update(entity_uuid.as_bytes());
        let bytes = mac.finalize().into_bytes();
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes);
        id
    }

    /// Base64url (no padding) encoding of [`Self::opaque_id`], the on-the-wire lane key.
    pub fn opaque_id_b64(&self, entity_type: &str, entity_uuid: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.opaque_id(entity_type, entity_uuid))
    }

    /// Encrypt an entity's serialized changeset into a hub lane blob (ADR-042 §2/§6).
    ///
    /// `account_id` and `device_id` are bound into the AEAD AAD so a hub that moves or
    /// forges a blob across lanes makes decryption fail. The plaintext is padded to a
    /// size bucket before encryption to blunt size-based fingerprinting.
    pub fn seal_entity(
        &self,
        account_id: &[u8],
        opaque_id: &[u8; 32],
        device_id: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let cek = derive_cek(&self.adk, opaque_id)?;
        let aad = blob_aad(account_id, opaque_id, device_id);
        let padded = Zeroizing::new(pad_bucket(plaintext));
        aead_encrypt(&cek, &aad, &padded)
    }

    /// Decrypt a hub lane blob produced by [`Self::seal_entity`].
    pub fn open_entity(
        &self,
        account_id: &[u8],
        opaque_id: &[u8; 32],
        device_id: &[u8],
        blob: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let cek = derive_cek(&self.adk, opaque_id)?;
        let aad = blob_aad(account_id, opaque_id, device_id);
        let padded = Zeroizing::new(aead_decrypt(&cek, &aad, blob)?);
        unpad_bucket(&padded)
    }

    /// Seal this trousseau to a new device's X25519 identity for Path B enrollment
    /// (ADR-042 §14 F6/B). Only the holder of the matching X25519 secret can open it,
    /// and the 96-byte plaintext layout never leaves the module unsealed. The returned
    /// base64 blob may be relayed through the hub since it is encrypted to the device.
    pub fn seal_to_device(
        &self,
        recipient_x25519_public: &[u8; 32],
    ) -> Result<String, CryptoError> {
        let plaintext = self.serialize();
        sealed_blob::seal(recipient_x25519_public, &plaintext)
    }

    /// Seal the trousseau at rest under a device-local symmetric key, producing
    /// `nonce || ciphertext` (ADR-042 §14 client-persistence addendum). The key is the
    /// same `Argon2(library_uuid)`-derived root that protects `crypto_keys`, so the at-rest
    /// blob is bound to this device and unreadable without it. The 96-byte plaintext never
    /// leaves this module, mirroring [`Self::seal_to_device`] and [`wrap_bundle`].
    pub fn seal_at_rest(&self, device_local_key: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
        let plaintext = self.serialize();
        aead_encrypt(device_local_key, BUNDLE_AAD_AT_REST, &plaintext)
    }

    /// Reload a trousseau sealed by [`Self::seal_at_rest`]. A wrong device-local key
    /// (e.g. a `library_uuid` storage swing) or any tampering fails the AEAD and returns
    /// [`CryptoError::DecryptionFailed`] — never a silently wrong bundle.
    pub fn open_at_rest(
        device_local_key: &[u8; 32],
        sealed: &[u8],
    ) -> Result<AccountKeyBundle, CryptoError> {
        let plaintext = Zeroizing::new(aead_decrypt(device_local_key, BUNDLE_AAD_AT_REST, sealed)?);
        AccountKeyBundle::deserialize(&plaintext)
    }

    /// Sign the canonical public account descriptor with the account auth key, producing the
    /// 64-byte `descriptor_sig` published at signup (ADR-042 §3). A joining device verifies it
    /// with [`verify_account_descriptor`] to confirm the public KDF/auth material it trusted
    /// was authored by the account key, not substituted by a malicious hub.
    pub fn sign_descriptor(&self, canonical: &[u8]) -> [u8; 64] {
        self.signing_key().sign(canonical).to_bytes()
    }

    /// Serialize the bundle to its fixed 96-byte plaintext layout (adk || aik || seed).
    /// The returned buffer is zeroized on drop.
    fn serialize(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(Vec::with_capacity(BUNDLE_PLAINTEXT_LEN));
        out.extend_from_slice(&self.adk);
        out.extend_from_slice(&self.aik);
        out.extend_from_slice(&self.account_auth_seed);
        out
    }

    /// Reconstruct a bundle from its 96-byte plaintext layout.
    fn deserialize(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != BUNDLE_PLAINTEXT_LEN {
            return Err(CryptoError::Serialization(
                "account bundle has unexpected length".to_string(),
            ));
        }
        let mut adk = [0u8; 32];
        let mut aik = [0u8; 32];
        let mut account_auth_seed = [0u8; 32];
        adk.copy_from_slice(&bytes[0..32]);
        aik.copy_from_slice(&bytes[32..64]);
        account_auth_seed.copy_from_slice(&bytes[64..96]);
        Ok(Self {
            adk,
            aik,
            account_auth_seed,
        })
    }
}

/// Derive the account Master Key from the passphrase via Argon2id (ADR-042 §3).
///
/// `params` MUST come from the account record on the read path, never be hard-coded,
/// so devices and the web client converge on the same MK and a future cost upgrade is
/// transparent. The MK is never stored or transmitted; it only derives KWK/AuthVerifier.
pub fn derive_master_key(
    passphrase: &[u8],
    salt: &[u8; 32],
    params: Argon2Params,
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let params = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
        .map_err(|_| CryptoError::KeyDerivationFailed)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut mk = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(passphrase, salt, mk.as_mut_slice())
        .map_err(|_| CryptoError::KeyDerivationFailed)?;
    Ok(mk)
}

/// Generate a 256-bit recovery key (ADR-042 §8). Shown to the user as a printable kit
/// and never sent to the hub; the caller renders it (e.g. BIP39, §14/L2).
pub fn generate_recovery_key() -> Zeroizing<[u8; 32]> {
    let mut rk = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(rk.as_mut_slice());
    rk
}

/// Key-Wrapping Key derived from the Master Key (wraps the trousseau, passphrase copy).
pub fn derive_kwk(mk: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    hkdf32(mk, None, HKDF_INFO_KWK)
}

/// Recovery Wrapping Key derived from the recovery key (wraps the trousseau, recovery copy).
pub fn derive_recovery_wrapping_key(rk: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    hkdf32(rk, None, HKDF_INFO_RWK)
}

/// AuthVerifier derived from the Master Key. The hub stores only a hash of it and uses
/// it to gate (rate-limit) trousseau download on the passphrase path (ADR-042 §5).
pub fn derive_auth_verifier(mk: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    hkdf32(mk, None, HKDF_INFO_AUTH_VERIFIER)
}

/// Wrap a bundle under `wrapping_key` (KWK or RWK), producing `nonce || ciphertext`.
/// The result is opaque to the hub.
pub fn wrap_bundle(
    bundle: &AccountKeyBundle,
    wrapping_key: &[u8; 32],
    kind: WrapKind,
) -> Result<Vec<u8>, CryptoError> {
    let plaintext = bundle.serialize();
    aead_encrypt(wrapping_key, kind.aad(), &plaintext)
}

/// Unwrap a bundle wrapped by [`wrap_bundle`]. A wrong passphrase/recovery key (wrong
/// wrapping key) or a `kind` mismatch fails the AEAD and returns [`CryptoError::DecryptionFailed`].
pub fn unwrap_bundle(
    wrapped: &[u8],
    wrapping_key: &[u8; 32],
    kind: WrapKind,
) -> Result<AccountKeyBundle, CryptoError> {
    let plaintext = Zeroizing::new(aead_decrypt(wrapping_key, kind.aad(), wrapped)?);
    AccountKeyBundle::deserialize(&plaintext)
}

/// Open a trousseau sealed by [`AccountKeyBundle::seal_to_device`] using this device's
/// X25519 static secret (Path B enrollment). A blob sealed to a different device, or a
/// tampered blob, fails the AEAD and returns an error.
pub fn open_device_sealed_bundle(
    my_x25519_secret: &StaticSecret,
    sealed_b64: &str,
) -> Result<AccountKeyBundle, CryptoError> {
    let plaintext = Zeroizing::new(sealed_blob::open(my_x25519_secret, sealed_b64)?);
    AccountKeyBundle::deserialize(&plaintext)
}

/// Domain-separated, length-framed canonical serialization of the PUBLIC account descriptor:
/// the exact bytes signed by `account_auth_sk` at signup and verified by a joining device
/// (ADR-042 §3). Fixed-size fields (salt, pk) are appended raw; the variable strings are
/// length-prefixed so the encoding is unambiguous regardless of their lengths. Both the
/// signer (signup) and verifier (enrollment, web client) MUST build these bytes identically.
#[allow(clippy::too_many_arguments)]
pub fn account_descriptor_canonical(
    account_salt: &[u8; 32],
    account_auth_pk: &[u8; 32],
    kdf_algo: &str,
    kdf_version: u32,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    schema_version: u32,
    auth_method: &str,
    aead_alg: &str,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(DESCRIPTOR_DOMAIN);
    out.extend_from_slice(account_salt);
    out.extend_from_slice(account_auth_pk);
    for n in [kdf_version, m_cost, t_cost, p_cost, schema_version] {
        out.extend_from_slice(&n.to_le_bytes());
    }
    for s in [kdf_algo, auth_method, aead_alg] {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    out
}

/// Verify a `descriptor_sig` over [`account_descriptor_canonical`] bytes against the account
/// auth public key. Returns `false` on any signature mismatch.
pub fn verify_account_descriptor(
    account_auth_pk: &VerifyingKey,
    canonical: &[u8],
    sig: &[u8; 64],
) -> bool {
    let signature = ed25519_dalek::Signature::from_bytes(sig);
    account_auth_pk.verify(canonical, &signature).is_ok()
}

// --- internal helpers ---

/// HKDF-SHA256 expand to 32 bytes (M5: IKM secret, optional context salt, label in info).
fn hkdf32(
    ikm: &[u8],
    salt: Option<&[u8]>,
    info: &[u8],
) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let hk = Hkdf::<Sha256>::new(salt, ikm);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(info, out.as_mut_slice())
        .map_err(|_| CryptoError::KeyDerivationFailed)?;
    Ok(out)
}

/// Per-entity content key: `HKDF(ikm=ADK, salt=opaque_id, info="...entity-content")`.
fn derive_cek(adk: &[u8; 32], opaque_id: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    hkdf32(adk, Some(opaque_id.as_slice()), HKDF_INFO_ENTITY_CONTENT)
}

/// Build the blob AEAD AAD with length-framed variable fields so the lane binding
/// is unambiguous regardless of `account_id`/`device_id` lengths (no concatenation
/// collision where `a1||oid||d1 == a2||oid||d2`):
/// `prefix || len(account_id) || account_id || opaque_id || len(device_id) || device_id`
/// (ADR-042 §2; §14/L6 adds account_id for defense in depth).
fn blob_aad(account_id: &[u8], opaque_id: &[u8; 32], device_id: &[u8]) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(BLOB_AAD_PREFIX.len() + 4 + account_id.len() + 32 + 4 + device_id.len());
    aad.extend_from_slice(BLOB_AAD_PREFIX);
    aad.extend_from_slice(&(account_id.len() as u32).to_le_bytes());
    aad.extend_from_slice(account_id);
    aad.extend_from_slice(opaque_id);
    aad.extend_from_slice(&(device_id.len() as u32).to_le_bytes());
    aad.extend_from_slice(device_id);
    aad
}

/// AES-256-GCM encrypt with AAD. Returns `nonce(12) || ciphertext`.
fn aead_encrypt(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::KeyDerivationFailed)?;
    let nonce_bytes = generate_nonce();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)?;
    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// AES-256-GCM decrypt with AAD of a `nonce(12) || ciphertext` buffer.
fn aead_decrypt(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < 12 {
        return Err(CryptoError::Serialization(
            "account blob shorter than nonce".to_string(),
        ));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::KeyDerivationFailed)?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)
}

/// Smallest padding bucket that fits `len` data bytes plus the 4-byte length prefix.
fn bucket_for(len: usize) -> usize {
    let total = len + 4;
    for &bucket in PAD_BUCKETS {
        if total <= bucket {
            return bucket;
        }
    }
    total.div_ceil(PAD_STEP_ABOVE) * PAD_STEP_ABOVE
}

/// Pad plaintext to a size bucket with a 4-byte little-endian length prefix (ADR-042 §6).
fn pad_bucket(data: &[u8]) -> Vec<u8> {
    let target = bucket_for(data.len());
    let mut out = Vec::with_capacity(target);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out.resize(target, 0);
    out
}

/// Strip the bucket padding added by [`pad_bucket`].
fn unpad_bucket(padded: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if padded.len() < 4 {
        return Err(CryptoError::Serialization(
            "padded blob too short".to_string(),
        ));
    }
    let len = u32::from_le_bytes(
        padded[..4]
            .try_into()
            .map_err(|_| CryptoError::Serialization("invalid length prefix".to_string()))?,
    ) as usize;
    if 4 + len > padded.len() {
        return Err(CryptoError::Serialization(
            "length prefix exceeds padded blob".to_string(),
        ));
    }
    Ok(padded[4..4 + len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, Verifier};

    // Cheap Argon2 params so the KDF tests stay fast; never used outside tests.
    fn fast_params() -> Argon2Params {
        Argon2Params {
            m_cost: 256,
            t_cost: 1,
            p_cost: 1,
        }
    }

    const ACCOUNT_ID: &[u8] = b"acct-123";
    const DEVICE_ID: &[u8] = b"device-aaaa";

    #[test]
    fn entity_seal_open_roundtrip() {
        let bundle = AccountKeyBundle::generate();
        let oid = bundle.opaque_id("book", "uuid-1");
        let plaintext = b"a cr-sqlite changeset for one book";

        let blob = bundle
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, plaintext)
            .unwrap();
        let opened = bundle
            .open_entity(ACCOUNT_ID, &oid, DEVICE_ID, &blob)
            .unwrap();

        assert_eq!(opened, plaintext);
    }

    #[test]
    fn blob_is_ciphertext_not_plaintext() {
        let bundle = AccountKeyBundle::generate();
        let oid = bundle.opaque_id("contact", "uuid-2");
        let plaintext = b"Alice Bookworm secret contact";

        let blob = bundle
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, plaintext)
            .unwrap();

        // The plaintext must not appear anywhere in the emitted blob.
        assert!(
            blob.windows(plaintext.len()).all(|w| w != plaintext),
            "plaintext leaked into the lane blob"
        );
    }

    #[test]
    fn aad_binding_rejects_lane_move() {
        let bundle = AccountKeyBundle::generate();
        let oid = bundle.opaque_id("book", "uuid-1");
        let blob = bundle
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, b"payload")
            .unwrap();

        // Same key material, but a different device_id (a moved lane) must fail.
        assert!(
            bundle
                .open_entity(ACCOUNT_ID, &oid, b"other-device", &blob)
                .is_err()
        );
        // A different account_id must fail too.
        assert!(
            bundle
                .open_entity(b"other-acct", &oid, DEVICE_ID, &blob)
                .is_err()
        );
    }

    #[test]
    fn tampered_blob_fails() {
        let bundle = AccountKeyBundle::generate();
        let oid = bundle.opaque_id("book", "uuid-1");
        let mut blob = bundle
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, b"payload")
            .unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(
            bundle
                .open_entity(ACCOUNT_ID, &oid, DEVICE_ID, &blob)
                .is_err()
        );
    }

    #[test]
    fn opaque_id_is_stable_and_type_separated() {
        let bundle = AccountKeyBundle::generate();
        let book = bundle.opaque_id("book", "shared-uuid");
        let book_again = bundle.opaque_id("book", "shared-uuid");
        let contact = bundle.opaque_id("contact", "shared-uuid");

        assert_eq!(book, book_again, "opaque_id must be deterministic");
        assert_ne!(
            book, contact,
            "same uuid under different types must not collide"
        );
    }

    #[test]
    fn opaque_id_differs_across_accounts() {
        let a = AccountKeyBundle::generate();
        let b = AccountKeyBundle::generate();
        // Different random AIK -> different opaque ids for the same entity.
        assert_ne!(a.opaque_id("book", "uuid-1"), b.opaque_id("book", "uuid-1"));
    }

    #[test]
    fn cek_differs_per_entity() {
        let bundle = AccountKeyBundle::generate();
        let oid1 = bundle.opaque_id("book", "uuid-1");
        let oid2 = bundle.opaque_id("book", "uuid-2");
        let cek1 = derive_cek(&bundle.adk, &oid1).unwrap();
        let cek2 = derive_cek(&bundle.adk, &oid2).unwrap();
        assert_ne!(*cek1, *cek2);
    }

    #[test]
    fn wrap_unwrap_passphrase_roundtrip() {
        let bundle = AccountKeyBundle::generate();
        let oid = bundle.opaque_id("book", "uuid-1");
        let salt = super::super::encryption::generate_salt();
        let mk = derive_master_key(b"correct horse battery staple", &salt, fast_params()).unwrap();
        let kwk = derive_kwk(&mk).unwrap();

        let wrapped = wrap_bundle(&bundle, &kwk, WrapKind::Passphrase).unwrap();
        let restored = unwrap_bundle(&wrapped, &kwk, WrapKind::Passphrase).unwrap();

        // The restored bundle must encrypt/decrypt identically to the original.
        let blob = restored
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, b"payload")
            .unwrap();
        assert_eq!(
            bundle
                .open_entity(ACCOUNT_ID, &oid, DEVICE_ID, &blob)
                .unwrap(),
            b"payload"
        );
    }

    #[test]
    fn wrong_passphrase_fails_to_unwrap() {
        let bundle = AccountKeyBundle::generate();
        let salt = super::super::encryption::generate_salt();
        let mk = derive_master_key(b"the right passphrase", &salt, fast_params()).unwrap();
        let kwk = derive_kwk(&mk).unwrap();
        let wrapped = wrap_bundle(&bundle, &kwk, WrapKind::Passphrase).unwrap();

        let wrong_mk = derive_master_key(b"the WRONG passphrase", &salt, fast_params()).unwrap();
        let wrong_kwk = derive_kwk(&wrong_mk).unwrap();
        assert!(unwrap_bundle(&wrapped, &wrong_kwk, WrapKind::Passphrase).is_err());
    }

    #[test]
    fn recovery_wrap_roundtrip_and_kind_mismatch_fails() {
        let bundle = AccountKeyBundle::generate();
        let rk = generate_recovery_key();
        let rwk = derive_recovery_wrapping_key(&rk).unwrap();

        let wrapped = wrap_bundle(&bundle, &rwk, WrapKind::Recovery).unwrap();
        assert!(unwrap_bundle(&wrapped, &rwk, WrapKind::Recovery).is_ok());

        // Right key, wrong kind label -> AAD mismatch -> failure.
        assert!(unwrap_bundle(&wrapped, &rwk, WrapKind::Passphrase).is_err());
    }

    #[test]
    fn master_key_is_deterministic() {
        let salt = super::super::encryption::generate_salt();
        let mk1 = derive_master_key(b"pass", &salt, fast_params()).unwrap();
        let mk2 = derive_master_key(b"pass", &salt, fast_params()).unwrap();
        assert_eq!(*mk1, *mk2);
    }

    #[test]
    fn generate_produces_distinct_keys() {
        let bundle = AccountKeyBundle::generate();
        // adk, aik, seed must not coincide (astronomically unlikely, guards a bad RNG wiring).
        assert_ne!(bundle.adk, bundle.aik);
        assert_ne!(bundle.adk, bundle.account_auth_seed);
        assert_ne!(bundle.aik, bundle.account_auth_seed);
    }

    #[test]
    fn account_auth_key_signs_and_verifies() {
        let bundle = AccountKeyBundle::generate();
        let pk = bundle.account_auth_pk();
        assert_eq!(pk, bundle.verifying_key().to_bytes());

        let challenge = b"hub-challenge-nonce";
        let sig = bundle.signing_key().sign(challenge);
        assert!(bundle.verifying_key().verify(challenge, &sig).is_ok());
    }

    #[test]
    fn padding_uses_buckets_and_roundtrips() {
        // Small payloads are floored to the 1 KiB bucket (M1).
        let small = pad_bucket(b"tiny");
        assert_eq!(small.len(), 1024);
        assert_eq!(unpad_bucket(&small).unwrap(), b"tiny");

        // A payload just over a bucket jumps to the next one.
        let data = vec![7u8; 1100];
        let padded = pad_bucket(&data);
        assert_eq!(padded.len(), 2048);
        assert_eq!(unpad_bucket(&padded).unwrap(), data);

        // Above the largest fixed bucket, sizes step by PAD_STEP_ABOVE.
        let big = vec![1u8; 20000];
        let padded_big = pad_bucket(&big);
        assert_eq!(padded_big.len() % PAD_STEP_ABOVE, 0);
        assert_eq!(unpad_bucket(&padded_big).unwrap(), big);
    }

    #[test]
    fn device_seal_open_roundtrip() {
        use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};
        let bundle = AccountKeyBundle::generate();
        let recipient = StaticSecret::random_from_rng(OsRng);
        let recipient_pub = X25519PublicKey::from(&recipient);

        let sealed = bundle.seal_to_device(recipient_pub.as_bytes()).unwrap();
        let restored = open_device_sealed_bundle(&recipient, &sealed).unwrap();

        // The restored trousseau is functionally identical to the original one.
        let oid = bundle.opaque_id("book", "uuid-1");
        let blob = restored
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, b"payload")
            .unwrap();
        assert_eq!(
            bundle
                .open_entity(ACCOUNT_ID, &oid, DEVICE_ID, &blob)
                .unwrap(),
            b"payload"
        );
        assert_eq!(restored.account_auth_pk(), bundle.account_auth_pk());
    }

    #[test]
    fn device_sealed_bundle_rejects_wrong_recipient() {
        use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};
        let bundle = AccountKeyBundle::generate();
        let recipient = StaticSecret::random_from_rng(OsRng);
        let recipient_pub = X25519PublicKey::from(&recipient);
        let eve = StaticSecret::random_from_rng(OsRng);

        let sealed = bundle.seal_to_device(recipient_pub.as_bytes()).unwrap();
        // A device that is not the sealed recipient cannot open the trousseau.
        assert!(open_device_sealed_bundle(&eve, &sealed).is_err());
    }

    #[test]
    fn at_rest_seal_open_roundtrip() {
        let bundle = AccountKeyBundle::generate();
        let key = [0x42u8; 32]; // stands in for the Argon2(library_uuid) device-local key.

        let sealed = bundle.seal_at_rest(&key).unwrap();
        // The sealed blob must not contain the raw key material in the clear.
        let plaintext = bundle.serialize();
        assert!(
            sealed.windows(plaintext.len()).all(|w| w != &plaintext[..]),
            "trousseau plaintext leaked into the at-rest blob"
        );

        let restored = AccountKeyBundle::open_at_rest(&key, &sealed).unwrap();
        // The reloaded trousseau is functionally identical to the original one.
        let oid = bundle.opaque_id("book", "uuid-1");
        let blob = restored
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, b"payload")
            .unwrap();
        assert_eq!(
            bundle
                .open_entity(ACCOUNT_ID, &oid, DEVICE_ID, &blob)
                .unwrap(),
            b"payload"
        );
        assert_eq!(restored.account_auth_pk(), bundle.account_auth_pk());
    }

    #[test]
    fn at_rest_rejects_wrong_device_local_key() {
        let bundle = AccountKeyBundle::generate();
        let sealed = bundle.seal_at_rest(&[1u8; 32]).unwrap();
        // A different device-local key (e.g. a library_uuid storage swing) must fail
        // the AEAD rather than yield a wrong bundle.
        assert!(AccountKeyBundle::open_at_rest(&[2u8; 32], &sealed).is_err());
    }

    #[test]
    fn descriptor_sign_verify_roundtrip_and_tamper() {
        let bundle = AccountKeyBundle::generate();
        let salt = [3u8; 32];
        let pk = bundle.account_auth_pk();
        let canonical = account_descriptor_canonical(
            &salt,
            &pk,
            "argon2id",
            19,
            65536,
            3,
            1,
            1,
            "passphrase",
            "AES-256-GCM",
        );
        let sig = bundle.sign_descriptor(&canonical);
        assert!(verify_account_descriptor(
            &bundle.verifying_key(),
            &canonical,
            &sig
        ));
        // A different account key (a malicious hub forging the descriptor) does not verify.
        let other = AccountKeyBundle::generate();
        assert!(!verify_account_descriptor(
            &other.verifying_key(),
            &canonical,
            &sig
        ));
        // Tampering a single descriptor field (here the Argon2 memory cost) breaks the sig.
        let tampered = account_descriptor_canonical(
            &salt,
            &pk,
            "argon2id",
            19,
            19456,
            3,
            1,
            1,
            "passphrase",
            "AES-256-GCM",
        );
        assert!(!verify_account_descriptor(
            &bundle.verifying_key(),
            &tampered,
            &sig
        ));
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let bundle = AccountKeyBundle::generate();
        let oid = bundle.opaque_id("tag", "uuid-empty");
        let blob = bundle
            .seal_entity(ACCOUNT_ID, &oid, DEVICE_ID, b"")
            .unwrap();
        assert_eq!(
            bundle
                .open_entity(ACCOUNT_ID, &oid, DEVICE_ID, &blob)
                .unwrap(),
            b""
        );
    }
}
