//! Passphrase rotation from an already-enrolled device (ADR-042 section 7 and 16.2,
//! lot B).
//!
//! The trousseau (ADK / AIK / `account_auth_sk`) is random and only ever WRAPPED under
//! the passphrase-derived key, so changing the passphrase re-wraps that one small blob
//! and nothing else: no entity blob is re-encrypted, the `kind=recovery` copy and its
//! marker are untouched, and every other enrolled device keeps working because each
//! holds the same trousseau sealed at rest under its own device root (section 15).
//!
//! ```text
//! new passphrase --check_passphrase (section 12 floor)-->
//!   salt' (OsRng) + Argon2id --> MK' --HKDF--> KWK'          re-wrap the SAME bundle
//!                                   \--HKDF--> AuthVerifier'  new keybundle-gate key
//!   descriptor_sig' = sign(canonical(salt', params', pk, ...))
//!   POST /passphrase  [bearer + fresh `rotate` challenge signed by account_auth_sk]
//! ```
//!
//! What this deliberately does NOT need: the OLD passphrase. The device already holds
//! the unlocked trousseau, so the rotation is authenticated by `account_auth_sk`, the
//! only credential a device that forgot its passphrase still has. That also means the
//! rotation is NOT a remedy against a device that already holds the trousseau (a lost
//! phone that was signed in, or a hub that kept the old wrapped copy next to a leaked
//! old passphrase, section 14/M3): those need a device removal or a full re-key.
//! Pure orchestration over the account-key crypto and the hub client; no persistence,
//! since nothing local changes.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::crypto::account_keys::{
    ACCOUNT_SCHEMA_VERSION, AEAD_ALG_V1, AccountKeyBundle, Argon2Params, WrapKind,
    account_descriptor_canonical, derive_auth_verifier, derive_kwk, derive_master_key, wrap_bundle,
};
use crate::crypto::encryption::generate_salt;
use crate::services::account_signup_service::{
    ARGON2_VERSION_0X13, AUTH_METHOD_PASSPHRASE, KDF_ALGO_ARGON2ID, PassphraseStrength,
    check_passphrase,
};
use crate::services::account_sync_client::{
    AccountSyncClient, AccountSyncError, KdfParams, PassphraseRotationRequest,
    encode_blob_standard, verifier_hash_hex,
};

#[derive(Debug)]
pub enum RotationError {
    /// The new passphrase did not clear the section 12 floor (score < 4 or length < 12).
    WeakPassphrase(PassphraseStrength),
    /// The hub refused the step-up: bad session, or a signature that does not verify
    /// against the account key (the trousseau this device holds is not the account's).
    AuthFailed,
    /// Crypto failure deriving keys or re-wrapping the trousseau.
    Crypto(String),
    /// Network or other hub error (a hub without the endpoint answers 404).
    Hub(String),
}

impl std::fmt::Display for RotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WeakPassphrase(_) => write!(f, "New passphrase is too weak"),
            Self::AuthFailed => write!(f, "Hub rejected the passphrase rotation"),
            Self::Crypto(e) => write!(f, "Crypto error: {e}"),
            Self::Hub(e) => write!(f, "Hub error: {e}"),
        }
    }
}

impl std::error::Error for RotationError {}

/// Build the new passphrase-side material for `bundle` under `new_passphrase`: fresh
/// salt, MK' (Argon2id on the blocking pool), KWK' re-wrap, AuthVerifier' hash, and the
/// descriptor re-signed. Pure (no network), so the crypto invariants are testable alone.
async fn build_rotation_material(
    bundle: &AccountKeyBundle,
    new_passphrase: &SecretString,
) -> Result<PassphraseRotationRequest, RotationError> {
    let salt = generate_salt();
    let params = Argon2Params::default();

    // Argon2id 64 MiB never runs inline on the single-threaded FFI runtime (same
    // pattern as signup and enrollment).
    let passphrase_bytes: Zeroizing<Vec<u8>> =
        Zeroizing::new(new_passphrase.expose_secret().as_bytes().to_vec());
    let mk = tokio::task::spawn_blocking(move || {
        derive_master_key(passphrase_bytes.as_slice(), &salt, params)
    })
    .await
    .map_err(|e| RotationError::Crypto(format!("key derivation task failed: {e}")))?
    .map_err(|e| RotationError::Crypto(e.to_string()))?;

    let kwk = derive_kwk(&mk).map_err(|e| RotationError::Crypto(e.to_string()))?;
    let auth_verifier =
        derive_auth_verifier(&mk).map_err(|e| RotationError::Crypto(e.to_string()))?;
    // The SAME trousseau, only its passphrase wrap changes (F2: never re-encrypt blobs).
    let wrapped = wrap_bundle(bundle, &kwk, WrapKind::Passphrase)
        .map_err(|e| RotationError::Crypto(e.to_string()))?;

    // A joining device verifies the descriptor signature over the salt/params it is
    // served (section 15.4), so the new salt must be signed by the account key.
    let canonical = account_descriptor_canonical(
        &salt,
        &bundle.account_auth_pk(),
        KDF_ALGO_ARGON2ID,
        ARGON2_VERSION_0X13,
        params.m_cost,
        params.t_cost,
        params.p_cost,
        ACCOUNT_SCHEMA_VERSION,
        AUTH_METHOD_PASSPHRASE,
        AEAD_ALG_V1,
    );
    let descriptor_sig = bundle.sign_descriptor(&canonical);

    Ok(PassphraseRotationRequest {
        account_salt: URL_SAFE_NO_PAD.encode(salt),
        kdf_params: KdfParams {
            algo: KDF_ALGO_ARGON2ID.to_string(),
            version: ARGON2_VERSION_0X13,
            m: params.m_cost,
            t: params.t_cost,
            p: params.p_cost,
        },
        auth_verifier_hash: verifier_hash_hex(&auth_verifier),
        descriptor_sig: URL_SAFE_NO_PAD.encode(descriptor_sig),
        wrapped_key: encode_blob_standard(&wrapped),
    })
}

/// Rotate the account passphrase from this device. `client` must hold an authenticated
/// session for `email` and `bundle` must be the unlocked trousseau of that account; the
/// old passphrase is never asked for. On success the hub serves the new material to the
/// next path A enrollment, and nothing changes locally.
pub async fn rotate_passphrase(
    client: &AccountSyncClient,
    email: &str,
    bundle: &AccountKeyBundle,
    new_passphrase: &SecretString,
) -> Result<(), RotationError> {
    // 1. Hard gate: the section 12 policy applies to a change exactly as to creation,
    //    before any crypto or network.
    let strength = check_passphrase(new_passphrase.expose_secret());
    if !strength.acceptable {
        return Err(RotationError::WeakPassphrase(strength));
    }

    // 2. Re-wrap under the new passphrase and re-sign the descriptor.
    let material = build_rotation_material(bundle, new_passphrase).await?;

    // 3. Publish: bearer session plus a fresh `rotate` challenge signed by the trousseau.
    client
        .rotate_passphrase(email, bundle, &material)
        .await
        .map_err(map_hub_err)
}

fn map_hub_err(e: AccountSyncError) -> RotationError {
    match e {
        AccountSyncError::Hub(401, _) => RotationError::AuthFailed,
        AccountSyncError::NotAuthenticated => RotationError::AuthFailed,
        other => RotationError::Hub(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::account_keys::{
        derive_recovery_wrapping_key, generate_recovery_key, unwrap_bundle,
        verify_account_descriptor,
    };
    use crate::services::account_enrollment::{EnrollmentError, enroll_with_passphrase};
    use crate::services::account_sync_client::decode_blob_standard;
    use ed25519_dalek::Verifier;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const OLD: &str = "correct horse battery staple";
    const NEW: &str = "purple giraffe reading seven lanterns";
    const EMAIL: &str = "reader@example.org";
    const TOKEN: &str = "sess-rotation";

    /// What the hub persists for one account: exactly the columns a rotation may or may
    /// not touch. Shared by the mock endpoints so the tests observe real state changes.
    struct HubState {
        salt: [u8; 32],
        kdf: KdfParams,
        auth_verifier_hash: String,
        descriptor_sig: [u8; 64],
        account_auth_pk: [u8; 32],
        wrapped_passphrase: Vec<u8>,
        wrapped_recovery: Vec<u8>,
        recovery_verifier_hash: String,
        /// Purpose of the last challenge issued (the rotation must ask for `rotate`).
        last_challenge_purpose: Option<String>,
        keybundle_calls: u32,
    }

    /// The nonce every challenge returns; the mock endpoints verify signatures/MACs
    /// against it so a wrong key or wrong verifier is really refused.
    const CHALLENGE: [u8; 32] = [7u8; 32];

    fn challenge_b64() -> String {
        URL_SAFE_NO_PAD.encode(CHALLENGE)
    }

    fn canonical_for(salt: &[u8; 32], pk: &[u8; 32], kdf: &KdfParams) -> Vec<u8> {
        account_descriptor_canonical(
            salt,
            pk,
            &kdf.algo,
            kdf.version,
            kdf.m,
            kdf.t,
            kdf.p,
            ACCOUNT_SCHEMA_VERSION,
            AUTH_METHOD_PASSPHRASE,
            AEAD_ALG_V1,
        )
    }

    /// A signed-up account: the trousseau plus the hub state its signup produced.
    fn signed_up_account() -> (AccountKeyBundle, HubState) {
        let bundle = AccountKeyBundle::generate();
        let salt = generate_salt();
        let params = Argon2Params::default();
        let kdf = KdfParams {
            algo: KDF_ALGO_ARGON2ID.into(),
            version: ARGON2_VERSION_0X13,
            m: params.m_cost,
            t: params.t_cost,
            p: params.p_cost,
        };
        let mk = derive_master_key(OLD.as_bytes(), &salt, params).unwrap();
        let kwk = derive_kwk(&mk).unwrap();
        let rk = generate_recovery_key();
        let rwk = derive_recovery_wrapping_key(&rk).unwrap();
        let pk = bundle.account_auth_pk();
        let state = HubState {
            salt,
            descriptor_sig: bundle.sign_descriptor(&canonical_for(&salt, &pk, &kdf)),
            kdf,
            auth_verifier_hash: verifier_hash_hex(&derive_auth_verifier(&mk).unwrap()),
            account_auth_pk: pk,
            wrapped_passphrase: wrap_bundle(&bundle, &kwk, WrapKind::Passphrase).unwrap(),
            wrapped_recovery: wrap_bundle(&bundle, &rwk, WrapKind::Recovery).unwrap(),
            recovery_verifier_hash: "marker-from-lot-a".to_string(),
            last_challenge_purpose: None,
            keybundle_calls: 0,
        };
        (bundle, state)
    }

    fn descriptor_json(st: &HubState) -> serde_json::Value {
        serde_json::json!({
            "account_salt": URL_SAFE_NO_PAD.encode(st.salt),
            "kdf_params": st.kdf,
            "schema_version": ACCOUNT_SCHEMA_VERSION,
            "auth_method": AUTH_METHOD_PASSPHRASE,
            "aead_alg": AEAD_ALG_V1,
            "account_auth_pk": URL_SAFE_NO_PAD.encode(st.account_auth_pk),
            "descriptor_sig": URL_SAFE_NO_PAD.encode(st.descriptor_sig),
        })
    }

    fn signature_verifies(pk: &[u8; 32], sig_b64: &str) -> bool {
        let Ok(sig) = URL_SAFE_NO_PAD.decode(sig_b64) else {
            return false;
        };
        let Ok(sig) = ed25519_dalek::Signature::from_slice(&sig) else {
            return false;
        };
        ed25519_dalek::VerifyingKey::from_bytes(pk)
            .unwrap()
            .verify(&CHALLENGE, &sig)
            .is_ok()
    }

    /// A minimal stateful hub: bootstrap / challenge / keybundle (MAC-gated) / login
    /// (Ed25519-gated) / passphrase rotation (bearer + Ed25519-gated). Mirrors the PHP
    /// gates so the end-to-end invariants below are real, not declared.
    async fn mount_hub(server: &MockServer, state: Arc<Mutex<HubState>>) {
        let st = Arc::clone(&state);
        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(move |_: &Request| {
                ResponseTemplate::new(200).set_body_json(descriptor_json(&st.lock().unwrap()))
            })
            .mount(server)
            .await;

        let st = Arc::clone(&state);
        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(move |req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                st.lock().unwrap().last_challenge_purpose =
                    body["purpose"].as_str().map(|s| s.to_string());
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "challenge": challenge_b64(),
                    "expires_at": "2026-01-01T00:00:00Z",
                }))
            })
            .mount(server)
            .await;

        let st = Arc::clone(&state);
        Mock::given(method("POST"))
            .and(path("/api/account/keybundle"))
            .respond_with(move |req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let mut st = st.lock().unwrap();
                st.keybundle_calls += 1;
                // PHP: hash_hmac('sha256', challenge, auth_verifier_hash), hex.
                let mut mac =
                    <Hmac<Sha256> as Mac>::new_from_slice(st.auth_verifier_hash.as_bytes())
                        .unwrap();
                mac.update(challenge_b64().as_bytes());
                let expected = hex::encode(mac.finalize().into_bytes());
                if body["mac"].as_str() != Some(expected.as_str()) {
                    return ResponseTemplate::new(401)
                        .set_body_json(serde_json::json!({"error": "Authentication failed"}));
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "wrapped_keys": [{
                        "kind": "passphrase",
                        "blob": encode_blob_standard(&st.wrapped_passphrase),
                    }],
                }))
            })
            .mount(server)
            .await;

        let st = Arc::clone(&state);
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(move |req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let st = st.lock().unwrap();
                if !signature_verifies(
                    &st.account_auth_pk,
                    body["signature"].as_str().unwrap_or(""),
                ) {
                    return ResponseTemplate::new(401)
                        .set_body_json(serde_json::json!({"error": "Authentication failed"}));
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "token": TOKEN,
                    "account_id": "acct-rot",
                    "descriptor": descriptor_json(&st),
                }))
            })
            .mount(server)
            .await;

        let st = Arc::clone(&state);
        Mock::given(method("POST"))
            .and(path("/api/account/passphrase"))
            .respond_with(move |req: &Request| {
                let bearer = req
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                let mut st = st.lock().unwrap();
                // Bearer + fresh challenge signed by the ACCOUNT key: the gate never
                // looks at the old verifier.
                if bearer != format!("Bearer {TOKEN}")
                    || !signature_verifies(
                        &st.account_auth_pk,
                        body["signature"].as_str().unwrap_or(""),
                    )
                {
                    return ResponseTemplate::new(401)
                        .set_body_json(serde_json::json!({"error": "Authentication failed"}));
                }
                let salt: [u8; 32] = URL_SAFE_NO_PAD
                    .decode(body["account_salt"].as_str().unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap();
                st.salt = salt;
                st.kdf = serde_json::from_value(body["kdf_params"].clone()).unwrap();
                st.auth_verifier_hash = body["auth_verifier_hash"].as_str().unwrap().to_string();
                st.descriptor_sig = URL_SAFE_NO_PAD
                    .decode(body["descriptor_sig"].as_str().unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap();
                st.wrapped_passphrase =
                    decode_blob_standard(body["wrapped_key"].as_str().unwrap()).unwrap();
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"status": "rotated"}))
            })
            .mount(server)
            .await;
    }

    /// An authenticated client for `bundle`, as a signed-in device holds one.
    async fn signed_in_client(server: &MockServer, bundle: &AccountKeyBundle) -> AccountSyncClient {
        let mut client = AccountSyncClient::with_base_url(server.uri());
        client.login(EMAIL, bundle).await.unwrap();
        client
    }

    #[tokio::test]
    async fn weak_new_passphrase_is_refused_before_any_network() {
        let client = AccountSyncClient::with_base_url("http://127.0.0.1:1");
        let bundle = AccountKeyBundle::generate();
        let err = rotate_passphrase(&client, EMAIL, &bundle, &SecretString::new("short".into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, RotationError::WeakPassphrase(_)));
    }

    /// The core of section 7: the hub ends up with a NEW salt, verifier, descriptor
    /// signature and passphrase wrap, all consistent with each other and with the SAME
    /// trousseau, while the recovery copy and its marker are byte-for-byte untouched. The
    /// rotating device never presented the old passphrase, nor fetched the keybundle.
    #[tokio::test]
    async fn rotation_rewraps_the_same_trousseau_and_leaves_the_recovery_copy_alone() {
        let server = MockServer::start().await;
        let (bundle, state) = signed_up_account();
        let before_salt = state.salt;
        let before_verifier = state.auth_verifier_hash.clone();
        let before_recovery = state.wrapped_recovery.clone();
        let before_marker = state.recovery_verifier_hash.clone();
        let state = Arc::new(Mutex::new(state));
        mount_hub(&server, Arc::clone(&state)).await;
        let client = signed_in_client(&server, &bundle).await;

        rotate_passphrase(&client, EMAIL, &bundle, &SecretString::new(NEW.into()))
            .await
            .unwrap();

        let st = state.lock().unwrap();
        assert_eq!(st.last_challenge_purpose.as_deref(), Some("rotate"));
        assert_eq!(st.keybundle_calls, 0, "rotation must not need the old copy");
        assert_ne!(st.salt, before_salt);
        assert_ne!(st.auth_verifier_hash, before_verifier);
        // Untouched: the recovery wrap, its marker, and the account key.
        assert_eq!(st.wrapped_recovery, before_recovery);
        assert_eq!(st.recovery_verifier_hash, before_marker);
        assert_eq!(st.account_auth_pk, bundle.account_auth_pk());

        // The new descriptor is signed by the account key over the new salt/params.
        let canonical = canonical_for(&st.salt, &st.account_auth_pk, &st.kdf);
        assert!(verify_account_descriptor(
            &bundle.verifying_key(),
            &canonical,
            &st.descriptor_sig
        ));

        // The new wrap opens under the NEW passphrase, onto the SAME trousseau...
        let params = Argon2Params {
            m_cost: st.kdf.m,
            t_cost: st.kdf.t,
            p_cost: st.kdf.p,
        };
        let mk_new = derive_master_key(NEW.as_bytes(), &st.salt, params).unwrap();
        let restored = unwrap_bundle(
            &st.wrapped_passphrase,
            &derive_kwk(&mk_new).unwrap(),
            WrapKind::Passphrase,
        )
        .unwrap();
        assert_eq!(restored.account_auth_pk(), bundle.account_auth_pk());
        // ...and the new verifier is the one derived from that same MK'.
        assert_eq!(
            st.auth_verifier_hash,
            verifier_hash_hex(&derive_auth_verifier(&mk_new).unwrap())
        );
        // ...while the OLD passphrase no longer opens anything, even under the new salt.
        let mk_old = derive_master_key(OLD.as_bytes(), &st.salt, params).unwrap();
        assert!(
            unwrap_bundle(
                &st.wrapped_passphrase,
                &derive_kwk(&mk_old).unwrap(),
                WrapKind::Passphrase
            )
            .is_err()
        );
    }

    /// End to end through the real enrollment code: after a rotation, path A on a fresh
    /// device accepts only the new passphrase, and unlocks the original trousseau.
    #[tokio::test]
    async fn after_rotation_path_a_accepts_only_the_new_passphrase() {
        let server = MockServer::start().await;
        let (bundle, state) = signed_up_account();
        let state = Arc::new(Mutex::new(state));
        mount_hub(&server, Arc::clone(&state)).await;
        let client = signed_in_client(&server, &bundle).await;

        rotate_passphrase(&client, EMAIL, &bundle, &SecretString::new(NEW.into()))
            .await
            .unwrap();

        let mut fresh = AccountSyncClient::with_base_url(server.uri());
        let err = enroll_with_passphrase(&mut fresh, EMAIL, &SecretString::new(OLD.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, EnrollmentError::WrongPassphrase));
        assert!(!fresh.is_authenticated());

        let mut fresh = AccountSyncClient::with_base_url(server.uri());
        let enrolled = enroll_with_passphrase(&mut fresh, EMAIL, &SecretString::new(NEW.into()))
            .await
            .unwrap();
        assert_eq!(enrolled.bundle.account_auth_pk(), bundle.account_auth_pk());
        assert!(fresh.is_authenticated());
    }

    /// Another device enrolled BEFORE the rotation (its trousseau sealed at rest under
    /// its own device root, section 15) is not involved in the rotation and keeps
    /// unlocking and authenticating afterwards: MK is only a path A bootstrap secret.
    #[tokio::test]
    async fn other_enrolled_devices_keep_working_after_rotation() {
        use crate::services::account_session_service;
        use sea_orm::Database;

        let server = MockServer::start().await;
        let (bundle, state) = signed_up_account();
        let state = Arc::new(Mutex::new(state));
        mount_hub(&server, Arc::clone(&state)).await;

        // Device B: enrolled earlier, session persisted at rest under its library uuid.
        let db_b = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db_b)
            .await
            .unwrap();
        let mut copy_b = AccountSyncClient::with_base_url(server.uri());
        let unlocked_b = enroll_with_passphrase(&mut copy_b, EMAIL, &SecretString::new(OLD.into()))
            .await
            .unwrap();
        account_session_service::persist(
            &db_b,
            "lib-b",
            "acct-rot",
            EMAIL,
            "dev-b",
            &unlocked_b.bundle,
        )
        .await
        .unwrap();
        drop(unlocked_b);

        // Device A rotates.
        let client_a = signed_in_client(&server, &bundle).await;
        rotate_passphrase(&client_a, EMAIL, &bundle, &SecretString::new(NEW.into()))
            .await
            .unwrap();

        // Device B relaunches: its at-rest trousseau still opens, and still logs in.
        let session_b = account_session_service::load(&db_b, "lib-b")
            .await
            .unwrap()
            .expect("device B session present");
        assert_eq!(session_b.bundle.account_auth_pk(), bundle.account_auth_pk());
        let mut relaunched_b = AccountSyncClient::with_base_url(server.uri());
        relaunched_b
            .login(EMAIL, &session_b.bundle)
            .await
            .expect("device B authenticates after the rotation it did not take part in");
    }

    /// A device whose trousseau is NOT the account's (wrong key) is refused by the hub
    /// gate: the step-up signature does not verify, and the hub state is unchanged.
    #[tokio::test]
    async fn rotation_with_a_foreign_trousseau_is_refused_and_changes_nothing() {
        let server = MockServer::start().await;
        let (_bundle, state) = signed_up_account();
        let before_salt = state.salt;
        let before_wrap = state.wrapped_passphrase.clone();
        let state = Arc::new(Mutex::new(state));
        mount_hub(&server, Arc::clone(&state)).await;

        let intruder = AccountKeyBundle::generate();
        let client = AccountSyncClient::with_base_url(server.uri());
        // A stolen bearer token alone must not be enough: the step-up needs the key.
        client.set_token_for_test(TOKEN);
        let err = rotate_passphrase(&client, EMAIL, &intruder, &SecretString::new(NEW.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, RotationError::AuthFailed));

        let st = state.lock().unwrap();
        assert_eq!(st.salt, before_salt);
        assert_eq!(st.wrapped_passphrase, before_wrap);
    }

    /// A hub predating the endpoint answers 404 (a plain not-found route): surfaced as a
    /// hub error, never as a wrong passphrase or a silent success.
    #[tokio::test]
    async fn hub_without_the_endpoint_is_surfaced_as_a_hub_error() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "challenge": challenge_b64(),
                "expires_at": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/passphrase"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        client.set_token_for_test(TOKEN);
        let err = rotate_passphrase(&client, EMAIL, &bundle, &SecretString::new(NEW.into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, RotationError::Hub(_)));
    }
}
