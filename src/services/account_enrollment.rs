//! Account enrollment — joining an existing account on a new device (ST-05 Phase D2).
//!
//! This is the client orchestration that turns a passphrase into an unlocked account
//! trousseau on a fresh device, composed purely from Phase A crypto
//! ([`crate::crypto::account_keys`]) and the Phase B hub client
//! ([`AccountSyncClient`]). It owns no persistence and no FFI — the Phase F account
//! service will call it and store the result.
//!
//! Path A (passphrase) — the floor required for the web client (ADR-042 §14 F6/A):
//! ```text
//! bootstrap(email)            -> public descriptor (salt, Argon2id params, account_auth_pk)
//! derive_master_key(passphrase, salt, params)
//! derive_auth_verifier(MK)    -> hash -> download_keybundle gate (HMAC challenge-response)
//! derive_kwk(MK)              -> unwrap_bundle(wrapped, KWK)   [AEAD: wrong passphrase fails]
//! login(bundle)              -> opaque account_id + session ready for sync
//! ```
//!
//! No secret ever leaves the device: the hub only sees the AuthVerifier *hash*, public
//! key material, and ciphertext. A wrong passphrase derives a wrong Master Key, so the
//! keybundle gate rejects it (HTTP 401) and, even if it did not, the AEAD unwrap fails —
//! both map to [`EnrollmentError::WrongPassphrase`].
//!
//! Path B ([`enroll_from_sealed_bundle`]) is the default-UX alternative (ADR-042 §14
//! F6/B): an already-authorized device seals the trousseau to this device's X25519
//! identity (the new device's public key travels over an authenticated channel — QR/SAS
//! — never relayed raw, §14/H2), so no passphrase or KDF work is needed here. Wiring the
//! new device into the signed device registry (adopt + re-publish) lands in a later slice.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::crypto::account_keys::{
    ACCOUNT_SCHEMA_VERSION, AEAD_ALG_V1, AccountKeyBundle, Argon2Params, WrapKind,
    account_descriptor_canonical, derive_auth_verifier, derive_kwk, derive_master_key,
    open_device_sealed_bundle, unwrap_bundle, verify_account_descriptor,
};
use crate::crypto::identity::NodeIdentity;
use crate::services::account_sync_client::{
    AccountDescriptor, AccountSyncClient, AccountSyncError, KdfParams, auth_verifier_hash_hex,
    decode_blob_standard,
};

/// Argon2 version 0x13 (the only version this client derives, V0x13). The hub publishes
/// it as the integer `19` in [`KdfParams::version`]; any other value is a refused downgrade.
const ARGON2_VERSION_0X13: u32 = 0x13;

/// The KDF algorithm this client supports for the account Master Key.
const KDF_ALGO_ARGON2ID: &str = "argon2id";

// Minimum Argon2id cost this client accepts from a downloaded account descriptor
// (SECURITY_GUIDELINES F3). The descriptor's KDF params come from the hub, so a
// malicious or compromised hub must not be able to downgrade them to make offline
// passphrase brute-force cheap. The floor is the current production baseline
// (account_keys: 64 MiB / t=3 / p=1) but is kept as a SEPARATE, stable minimum so
// accounts created at the baseline stay enrollable if production later raises its
// own cost. p is pinned exactly (WASM single-thread parity, ST-07).
const MIN_ARGON2_M_COST: u32 = 65536; // 64 MiB
const MIN_ARGON2_T_COST: u32 = 3;
const REQUIRED_ARGON2_P_COST: u32 = 1;

/// Outcome of a successful Path A enrollment: the unlocked trousseau plus the opaque
/// account id needed to key blobs. On return, `client` holds an authenticated session.
pub struct EnrolledAccount {
    /// Opaque hub account id (bound into the per-blob AEAD AAD during sync).
    pub account_id: String,
    /// The unlocked account trousseau, decrypted only in RAM (never logged, zeroized on drop).
    pub bundle: AccountKeyBundle,
}

#[derive(Debug)]
pub enum EnrollmentError {
    /// No account exists for this email on the hub.
    AccountNotFound,
    /// The passphrase did not unlock the trousseau (gate rejection or AEAD failure).
    WrongPassphrase,
    /// The sealed trousseau could not be opened with this device's identity (Path B).
    SealedBundleInvalid,
    /// The hub rejected authentication with the unlocked trousseau (wrong account).
    AuthFailed,
    /// The hub published a KDF/AEAD/schema profile this client refuses (no downgrade).
    UnsupportedProfile(String),
    /// The hub omitted the requested wrapped key copy.
    MissingWrappedKey,
    /// The unlocked trousseau does not match the public account descriptor.
    DescriptorMismatch,
    /// The public descriptor's signature does not verify: the hub tampered with the salt /
    /// KDF params / auth material it served (or supplied a bad signature).
    DescriptorSigInvalid,
    /// Malformed base64 in a hub-supplied field (salt, key, blob).
    Encoding(String),
    /// Crypto failure deriving keys or unwrapping the trousseau.
    Crypto(String),
    /// Network or non-auth hub error.
    Hub(String),
}

impl std::fmt::Display for EnrollmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccountNotFound => write!(f, "No account exists for this email"),
            Self::WrongPassphrase => write!(f, "Incorrect passphrase"),
            Self::SealedBundleInvalid => {
                write!(f, "Sealed trousseau could not be opened on this device")
            }
            Self::AuthFailed => write!(f, "Hub rejected authentication with this trousseau"),
            Self::UnsupportedProfile(e) => write!(f, "Unsupported account profile: {e}"),
            Self::MissingWrappedKey => write!(f, "Hub returned no passphrase-wrapped key"),
            Self::DescriptorMismatch => {
                write!(f, "Account key does not match the published descriptor")
            }
            Self::DescriptorSigInvalid => {
                write!(f, "Account descriptor signature did not verify")
            }
            Self::Encoding(e) => write!(f, "Encoding error: {e}"),
            Self::Crypto(e) => write!(f, "Crypto error: {e}"),
            Self::Hub(e) => write!(f, "Hub error: {e}"),
        }
    }
}

impl std::error::Error for EnrollmentError {}

/// Path A enrollment: join an existing account on this device using its passphrase.
///
/// On success the trousseau is unlocked in RAM and `client` is left authenticated. The
/// passphrase is only ever fed to Argon2id and is never sent to the hub.
pub async fn enroll_with_passphrase(
    client: &mut AccountSyncClient,
    email: &str,
    passphrase: &SecretString,
) -> Result<EnrolledAccount, EnrollmentError> {
    // 1. Fetch the account's public KDF descriptor (404 => no such account).
    let descriptor = client.bootstrap(email).await.map_err(map_bootstrap_err)?;

    // 2. Pin the published profile: refuse a hub that serves a weaker/different KDF,
    //    AEAD, or schema than this client implements (ADR-042 §14/M5, no downgrade).
    let params = validate_profile(&descriptor)?;

    // 2b. Verify the descriptor signature: the salt + KDF/auth material must have been
    //     signed by the account auth key, so a malicious hub cannot silently swap the salt
    //     or auth pk it serves (ADR-042 §3). Defense in depth above validate_profile (which
    //     pins the params) and the post-unwrap descriptor match (which binds the auth key).
    verify_descriptor_signature(&descriptor)?;

    // 3. Derive the Master Key from the passphrase and the published salt/params.
    //    Argon2id (64 MiB, t=3) dominates the cost (~hundreds of ms); run it on the
    //    blocking pool so it never stalls the single-threaded FFI runtime, matching the
    //    pattern already used for password-derived backup keys (see `api/backup.rs`).
    let salt = decode_b64url_32(&descriptor.account_salt)?;
    let passphrase_bytes: Zeroizing<Vec<u8>> =
        Zeroizing::new(passphrase.expose_secret().as_bytes().to_vec());
    let mk = tokio::task::spawn_blocking(move || {
        derive_master_key(passphrase_bytes.as_slice(), &salt, params)
    })
    .await
    .map_err(|e| EnrollmentError::Crypto(format!("key derivation task failed: {e}")))?
    .map_err(|e| EnrollmentError::Crypto(e.to_string()))?;

    // 4. Gate-download the wrapped trousseau with the AuthVerifier HMAC (401 => wrong
    //    passphrase, since a wrong MK yields a wrong verifier hash).
    let auth_verifier =
        derive_auth_verifier(&mk).map_err(|e| EnrollmentError::Crypto(e.to_string()))?;
    let verifier_hash = auth_verifier_hash_hex(&auth_verifier);
    let wrapped_keys = client
        .download_keybundle(email, &verifier_hash, &[WrapKind::Passphrase.wire_kind()])
        .await
        .map_err(map_keybundle_err)?;
    let wrapped = wrapped_keys
        .iter()
        .find(|k| k.kind == WrapKind::Passphrase.wire_kind())
        .ok_or(EnrollmentError::MissingWrappedKey)?;
    let wrapped_bytes = decode_blob_standard(&wrapped.blob)
        .map_err(|e| EnrollmentError::Encoding(e.to_string()))?;

    // 5. Unwrap the trousseau locally. A right MK but wrong wrapped bytes (or a hub that
    //    served a tampered salt/params making the KWK wrong) fails the AEAD here.
    let kwk = derive_kwk(&mk).map_err(|e| EnrollmentError::Crypto(e.to_string()))?;
    let bundle = unwrap_bundle(&wrapped_bytes, &kwk, WrapKind::Passphrase)
        .map_err(|_| EnrollmentError::WrongPassphrase)?;

    // 6. Integrity: the unlocked auth key must match the descriptor we trusted for the
    //    KDF params, catching a hub that mismatched the public material.
    let descriptor_pk = decode_b64url_32(&descriptor.account_auth_pk)?;
    if bundle.account_auth_pk() != descriptor_pk {
        return Err(EnrollmentError::DescriptorMismatch);
    }

    // 7. Authenticate so the session is ready for sync and learn the opaque account id.
    //    Login also proves to the hub that this trousseau owns the account's auth key.
    let outcome = client.login(email, &bundle).await.map_err(map_login_err)?;

    Ok(EnrolledAccount {
        account_id: outcome.account_id,
        bundle,
    })
}

/// Path B enrollment: join an existing account on this device using a trousseau that an
/// already-authorized device sealed to this device's X25519 identity (ADR-042 §14 F6/B).
///
/// No passphrase or KDF work is involved: the sealed blob carries the trousseau directly.
/// On success the trousseau is unlocked in RAM and `client` is left authenticated. The
/// new device's public key must have reached the sealing device over an authenticated
/// channel (QR/SAS); this function only consumes the resulting sealed blob.
pub async fn enroll_from_sealed_bundle(
    client: &mut AccountSyncClient,
    email: &str,
    my_identity: &NodeIdentity,
    sealed_bundle_b64: &str,
) -> Result<EnrolledAccount, EnrollmentError> {
    // 1. Open the sealed trousseau with this device's X25519 secret (a blob sealed to a
    //    different device, or a tampered one, fails the AEAD here).
    let bundle = open_device_sealed_bundle(my_identity.x25519_static_secret(), sealed_bundle_b64)
        .map_err(|_| EnrollmentError::SealedBundleInvalid)?;

    // 2. Authenticate: login proves to the hub that this trousseau owns the account's
    //    auth key, and yields the opaque account id needed to key blobs during sync.
    let outcome = client.login(email, &bundle).await.map_err(map_login_err)?;

    Ok(EnrolledAccount {
        account_id: outcome.account_id,
        bundle,
    })
}

/// Refuse any account profile this client cannot derive identically (ADR-042 §3/§14 M5).
fn validate_profile(descriptor: &AccountDescriptor) -> Result<Argon2Params, EnrollmentError> {
    let KdfParams {
        algo,
        version,
        m,
        t,
        p,
    } = &descriptor.kdf_params;
    if algo != KDF_ALGO_ARGON2ID {
        return Err(EnrollmentError::UnsupportedProfile(format!("KDF {algo}")));
    }
    if *version != ARGON2_VERSION_0X13 {
        return Err(EnrollmentError::UnsupportedProfile(format!(
            "Argon2 version {version}"
        )));
    }
    if descriptor.aead_alg != AEAD_ALG_V1 {
        return Err(EnrollmentError::UnsupportedProfile(format!(
            "AEAD {}",
            descriptor.aead_alg
        )));
    }
    if descriptor.schema_version != ACCOUNT_SCHEMA_VERSION {
        return Err(EnrollmentError::UnsupportedProfile(format!(
            "schema v{}",
            descriptor.schema_version
        )));
    }
    // KDF cost floor: refuse a hub-supplied profile weaker than the baseline, so a
    // downgrade cannot cheapen offline passphrase brute-force (SECURITY_GUIDELINES F3).
    if *m < MIN_ARGON2_M_COST || *t < MIN_ARGON2_T_COST || *p != REQUIRED_ARGON2_P_COST {
        return Err(EnrollmentError::UnsupportedProfile(format!(
            "KDF cost below floor (m={m}, t={t}, p={p})"
        )));
    }
    Ok(Argon2Params {
        m_cost: *m,
        t_cost: *t,
        p_cost: *p,
    })
}

/// Decode a base64url(no-pad) field the hub guarantees is 32 bytes (salt, public key).
fn decode_b64url_32(value: &str) -> Result<[u8; 32], EnrollmentError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| EnrollmentError::Encoding(e.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| EnrollmentError::Encoding("expected 32 bytes".to_string()))
}

/// Decode a base64url(no-pad) 64-byte Ed25519 signature field (`descriptor_sig`).
fn decode_b64url_64(value: &str) -> Result<[u8; 64], EnrollmentError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| EnrollmentError::Encoding(e.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| EnrollmentError::Encoding("expected 64 bytes".to_string()))
}

/// Verify the account descriptor signature (ADR-042 §3): rebuild the canonical descriptor
/// from the served fields and check it was signed by the descriptor's own account auth key.
/// A bad signature or a non-point public key is a [`EnrollmentError::DescriptorSigInvalid`].
fn verify_descriptor_signature(descriptor: &AccountDescriptor) -> Result<(), EnrollmentError> {
    let salt = decode_b64url_32(&descriptor.account_salt)?;
    let pk_bytes = decode_b64url_32(&descriptor.account_auth_pk)?;
    let sig = decode_b64url_64(&descriptor.descriptor_sig)?;
    let canonical = account_descriptor_canonical(
        &salt,
        &pk_bytes,
        &descriptor.kdf_params.algo,
        descriptor.kdf_params.version,
        descriptor.kdf_params.m,
        descriptor.kdf_params.t,
        descriptor.kdf_params.p,
        descriptor.schema_version,
        &descriptor.auth_method,
        &descriptor.aead_alg,
    );
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes)
        .map_err(|_| EnrollmentError::DescriptorSigInvalid)?;
    if !verify_account_descriptor(&vk, &canonical, &sig) {
        return Err(EnrollmentError::DescriptorSigInvalid);
    }
    Ok(())
}

fn map_bootstrap_err(e: AccountSyncError) -> EnrollmentError {
    match e {
        AccountSyncError::Hub(404, _) => EnrollmentError::AccountNotFound,
        other => EnrollmentError::Hub(other.to_string()),
    }
}

fn map_keybundle_err(e: AccountSyncError) -> EnrollmentError {
    match e {
        // The gate returns 401 both for a bad MAC (wrong passphrase) and an unknown
        // account; bootstrap already proved the account exists, so 401 means passphrase.
        AccountSyncError::Hub(401, _) => EnrollmentError::WrongPassphrase,
        other => EnrollmentError::Hub(other.to_string()),
    }
}

fn map_login_err(e: AccountSyncError) -> EnrollmentError {
    match e {
        AccountSyncError::Hub(401, _) => EnrollmentError::AuthFailed,
        AccountSyncError::Hub(404, _) => EnrollmentError::AccountNotFound,
        other => EnrollmentError::Hub(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::account_keys::{
        account_descriptor_canonical, derive_kwk, derive_master_key, wrap_bundle,
    };
    use crate::crypto::encryption::generate_salt;
    use crate::services::account_sync_client::encode_blob_standard;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Floor-valid Argon2 params (must match the mock hub's descriptor so the client
    // derives exactly the Master Key that wrapped the bundle). These are at the
    // accepted cost floor now that `validate_profile` enforces it, so happy-path
    // enrollment tests run a real 64 MiB derivation.
    fn valid_kdf_params() -> Argon2Params {
        Argon2Params {
            m_cost: MIN_ARGON2_M_COST,
            t_cost: MIN_ARGON2_T_COST,
            p_cost: REQUIRED_ARGON2_P_COST,
        }
    }

    const PASSPHRASE: &str = "correct horse battery staple";
    const EMAIL: &str = "reader@example.org";

    /// Build the wrapped passphrase copy a freshly signed-up account would store.
    fn wrap_for(bundle: &AccountKeyBundle, salt: &[u8; 32], passphrase: &str) -> String {
        let mk = derive_master_key(passphrase.as_bytes(), salt, valid_kdf_params()).unwrap();
        let kwk = derive_kwk(&mk).unwrap();
        let wrapped = wrap_bundle(bundle, &kwk, WrapKind::Passphrase).unwrap();
        encode_blob_standard(&wrapped)
    }

    /// Build a descriptor JSON signed by `signer` (so its `descriptor_sig` verifies against
    /// the `account_auth_pk` it advertises). To simulate a key mismatch, pass a `signer`
    /// different from the trousseau that wrapped the keybundle.
    fn descriptor_json(salt: &[u8; 32], signer: &AccountKeyBundle) -> serde_json::Value {
        let pk = signer.account_auth_pk();
        let canonical = account_descriptor_canonical(
            salt,
            &pk,
            "argon2id",
            19,
            MIN_ARGON2_M_COST,
            MIN_ARGON2_T_COST,
            REQUIRED_ARGON2_P_COST,
            1,
            "passphrase",
            "AES-256-GCM",
        );
        let sig = signer.sign_descriptor(&canonical);
        serde_json::json!({
            "account_salt": URL_SAFE_NO_PAD.encode(salt),
            "kdf_params": {"algo": "argon2id", "version": 19, "m": MIN_ARGON2_M_COST, "t": MIN_ARGON2_T_COST, "p": REQUIRED_ARGON2_P_COST},
            "schema_version": 1,
            "auth_method": "passphrase",
            "aead_alg": "AES-256-GCM",
            "account_auth_pk": URL_SAFE_NO_PAD.encode(pk),
            "descriptor_sig": URL_SAFE_NO_PAD.encode(sig),
        })
    }

    async fn mount_challenge(server: &MockServer) {
        // Both login and keybundle fetch a challenge from the same endpoint; a fixed
        // nonce works because the mock login/keybundle handlers do not verify it.
        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "challenge": URL_SAFE_NO_PAD.encode([7u8; 32]),
                "expires_at": "2026-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn path_a_enrollment_unlocks_bundle_and_authenticates() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        let wrapped_blob = wrap_for(&bundle, &salt, PASSPHRASE);

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor_json(&salt, &bundle)))
            .mount(&server)
            .await;
        mount_challenge(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/account/keybundle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "wrapped_keys": [{ "kind": "passphrase", "blob": wrapped_blob }],
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "sess-xyz",
                "account_id": "acct-42",
                "descriptor": descriptor_json(&salt, &bundle),
            })))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let enrolled =
            enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
                .await
                .unwrap();

        assert_eq!(enrolled.account_id, "acct-42");
        assert!(client.is_authenticated());
        // The unlocked trousseau is the real one: it carries the account auth key.
        assert_eq!(enrolled.bundle.account_auth_pk(), bundle.account_auth_pk());
    }

    #[tokio::test]
    async fn unknown_account_maps_to_account_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error": "Account not found"})),
            )
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::AccountNotFound));
    }

    #[tokio::test]
    async fn wrong_passphrase_gate_rejection_maps_to_wrong_passphrase() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor_json(&salt, &bundle)))
            .mount(&server)
            .await;
        mount_challenge(&server).await;
        // A wrong passphrase yields a wrong verifier hash, so the gate returns 401.
        Mock::given(method("POST"))
            .and(path("/api/account/keybundle"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error": "Authentication failed"})),
            )
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err =
            enroll_with_passphrase(&mut client, EMAIL, &SecretString::new("wrong pass".into()))
                .await
                .err()
                .unwrap();
        assert!(matches!(err, EnrollmentError::WrongPassphrase));
    }

    #[tokio::test]
    async fn tampered_wrapped_blob_fails_aead_as_wrong_passphrase() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        // Correct verifier hash (gate passes) but a wrapped blob that fails to unwrap.
        let mut wrapped = decode_blob_standard(&wrap_for(&bundle, &salt, PASSPHRASE)).unwrap();
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0xFF;
        let tampered_blob = encode_blob_standard(&wrapped);

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor_json(&salt, &bundle)))
            .mount(&server)
            .await;
        mount_challenge(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/account/keybundle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "wrapped_keys": [{ "kind": "passphrase", "blob": tampered_blob }],
            })))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::WrongPassphrase));
    }

    #[tokio::test]
    async fn mismatched_descriptor_key_is_rejected() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        let wrapped_blob = wrap_for(&bundle, &salt, PASSPHRASE);
        // The descriptor advertises (and is signed by) a DIFFERENT account key than the
        // trousseau holds, so its signature verifies but the post-unwrap key match fails.
        let other = AccountKeyBundle::generate();

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor_json(&salt, &other)))
            .mount(&server)
            .await;
        mount_challenge(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/account/keybundle"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "wrapped_keys": [{ "kind": "passphrase", "blob": wrapped_blob }],
            })))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::DescriptorMismatch));
    }

    #[tokio::test]
    async fn downgraded_kdf_profile_is_refused() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        let mut descriptor = descriptor_json(&salt, &bundle);
        descriptor["kdf_params"]["algo"] = serde_json::json!("pbkdf2");

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::UnsupportedProfile(_)));
    }

    #[tokio::test]
    async fn weak_kdf_cost_is_refused() {
        // A hostile hub serves a structurally valid descriptor but with a downgraded
        // Argon2 memory cost (256 KiB instead of the 64 MiB floor) to make offline
        // brute-force of the passphrase cheap. validate_profile must reject it before
        // any derivation, so the now-stale descriptor_sig is never even reached.
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        let mut descriptor = descriptor_json(&salt, &bundle);
        descriptor["kdf_params"]["m"] = serde_json::json!(256);

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::UnsupportedProfile(_)));
    }

    #[tokio::test]
    async fn tampered_descriptor_fails_signature_verification() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        // The hub serves a descriptor whose salt differs from what was signed: the
        // descriptor signature must fail before any key derivation happens.
        let mut descriptor = descriptor_json(&salt, &bundle);
        descriptor["account_salt"] = serde_json::json!(URL_SAFE_NO_PAD.encode(generate_salt()));

        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(ResponseTemplate::new(200).set_body_json(descriptor))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut client, EMAIL, &SecretString::new(PASSPHRASE.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::DescriptorSigInvalid));
    }

    // --- Path B (sealed X25519 transfer) ---

    #[tokio::test]
    async fn path_b_sealed_enrollment_unlocks_and_authenticates() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let account_pk = bundle.account_auth_pk();
        // The new device's identity; an authorized device seals the trousseau to it.
        let new_device = NodeIdentity::generate();
        let sealed = bundle
            .seal_to_device(new_device.x25519_public_key().as_bytes())
            .unwrap();

        mount_challenge(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "sess-b",
                "account_id": "acct-b",
                "descriptor": descriptor_json(&generate_salt(), &bundle),
            })))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let enrolled = enroll_from_sealed_bundle(&mut client, EMAIL, &new_device, &sealed)
            .await
            .unwrap();

        assert_eq!(enrolled.account_id, "acct-b");
        assert!(client.is_authenticated());
        assert_eq!(enrolled.bundle.account_auth_pk(), account_pk);
    }

    #[tokio::test]
    async fn path_b_rejects_bundle_sealed_to_another_device() {
        let bundle = AccountKeyBundle::generate();
        let intended = NodeIdentity::generate();
        let other_device = NodeIdentity::generate();
        let sealed = bundle
            .seal_to_device(intended.x25519_public_key().as_bytes())
            .unwrap();

        // The hub is never contacted: opening fails before any login attempt.
        let mut client = AccountSyncClient::with_base_url("http://127.0.0.1:1");
        let err = enroll_from_sealed_bundle(&mut client, EMAIL, &other_device, &sealed)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::SealedBundleInvalid));
        assert!(!client.is_authenticated());
    }

    #[tokio::test]
    async fn path_b_login_rejection_maps_to_auth_failed() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let new_device = NodeIdentity::generate();
        let sealed = bundle
            .seal_to_device(new_device.x25519_public_key().as_bytes())
            .unwrap();

        mount_challenge(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error": "Authentication failed"})),
            )
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_from_sealed_bundle(&mut client, EMAIL, &new_device, &sealed)
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::AuthFailed));
    }
}
