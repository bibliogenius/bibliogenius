//! Account creation (signup) orchestration (ST-05 Phase F).
//!
//! Creates a BRAND NEW account from a passphrase: generates a random trousseau, wraps it
//! under the passphrase Master Key AND a recovery key, signs the public descriptor, creates
//! the first signed device registry, and registers everything with the hub. Pure
//! orchestration over Phase A crypto ([`crate::crypto::account_keys`]) and the Phase B hub
//! client ([`AccountSyncClient`]); it owns no local persistence (the Phase F FFI layer
//! persists the returned session, exactly as for [`super::account_enrollment`]).
//!
//! Two security floors from `SECURITY_GUIDELINES.md` F7 are enforced here, never skipped:
//! - **Passphrase strength** ([`check_passphrase`]): zxcvbn score 4/4 AND length >= 12,
//!   100% local (no network). It is the only wall against an offline brute-force by a
//!   compromised hub.
//! - **Recovery kit** ([`SignupOutcome::recovery_phrase`]): a 256-bit recovery key, rendered
//!   as a 24-word BIP39 mnemonic, double-wraps the trousseau (ADR-042 §8). It is returned to
//!   be shown ONCE and never persisted; losing both passphrase and kit = permanent loss.
//!
//! Account-id ordering: the hub assigns a random `account_id` only in the signup response,
//! but the signed device registry embeds that id (for anti cross-account replay) and the
//! `SignupRequest` requires a registry blob up front. So signup publishes a placeholder
//! registry, then RE-PUBLISHES one signed with the real `account_id` once login yields it.
//! The placeholder is overwritten before any second device can join.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::crypto::account_keys::{
    ACCOUNT_SCHEMA_VERSION, AEAD_ALG_V1, AccountKeyBundle, Argon2Params, WrapKind,
    account_descriptor_canonical, derive_auth_verifier, derive_kwk, derive_master_key,
    derive_recovery_wrapping_key, generate_recovery_key, wrap_bundle,
};
use crate::crypto::device_registry::{DeviceEntry, DeviceRegistry};
use crate::crypto::encryption::generate_salt;
use crate::services::account_sync_client::{
    AccountSyncClient, AccountSyncError, KdfParams, SignupRequest, WrappedKeyDto,
    auth_verifier_hash_hex, encode_blob_standard,
};

/// Argon2 version 0x13 as the integer the hub stores (`19`).
const ARGON2_VERSION_0X13: u32 = 0x13;
/// The account KDF algorithm label.
const KDF_ALGO_ARGON2ID: &str = "argon2id";
/// The auth method this signup creates (passphrase-derived Master Key).
const AUTH_METHOD_PASSPHRASE: &str = "passphrase";

/// Minimum passphrase length at account creation (SECURITY_GUIDELINES F7 / ADR-042 §14 H4).
pub const MIN_PASSPHRASE_LEN: usize = 12;
/// Minimum zxcvbn score (0..4) at account creation (F7: the bar is the maximum, 4/4).
pub const MIN_ZXCVBN_SCORE: u8 = 4;

/// Stable FFI error prefix: an account already exists for this email. The Flutter layer
/// pattern-matches it to offer "sign in instead" (the user can join with their passphrase
/// via the enrollment path), instead of dead-ending on a duplicate-signup attempt.
pub const E_ACCOUNT_EXISTS: &str = "E_ACCOUNT_EXISTS";
/// Stable FFI error prefix: the passphrase did not clear the strength floor (backstop to the
/// live meter, which normally gates the button before signup is called).
pub const E_WEAK_PASSPHRASE: &str = "E_WEAK_PASSPHRASE";

/// Local strength assessment of a candidate passphrase. Computed 100% on-device.
#[derive(Debug, Clone)]
pub struct PassphraseStrength {
    /// zxcvbn score, 0 (weakest) to 4 (strongest).
    pub score: u8,
    pub length: usize,
    /// Whether it clears BOTH floors (score == 4 AND length >= 12).
    pub acceptable: bool,
    /// zxcvbn's primary warning, if any (English; the UI may localize by score).
    pub warning: Option<String>,
    /// zxcvbn's improvement suggestions (English).
    pub suggestions: Vec<String>,
}

/// Score and gate a candidate passphrase locally (no network). Used both for the live
/// strength meter and as the hard gate inside [`signup`].
pub fn check_passphrase(passphrase: &str) -> PassphraseStrength {
    let estimate = zxcvbn::zxcvbn(passphrase, &[]);
    let score = u8::from(estimate.score());
    let length = passphrase.chars().count();
    let (warning, suggestions) = match estimate.feedback() {
        Some(fb) => (
            fb.warning().map(|w| w.to_string()),
            fb.suggestions().iter().map(|s| s.to_string()).collect(),
        ),
        None => (None, Vec::new()),
    };
    PassphraseStrength {
        score,
        length,
        acceptable: score >= MIN_ZXCVBN_SCORE && length >= MIN_PASSPHRASE_LEN,
        warning,
        suggestions,
    }
}

/// Render a 256-bit recovery key as a 24-word BIP39 mnemonic (ADR-042 §14 L2).
fn recovery_phrase(recovery_key: &[u8; 32]) -> Result<String, SignupError> {
    bip39::Mnemonic::from_entropy(recovery_key)
        .map(|m| m.to_string())
        .map_err(|e| SignupError::Crypto(format!("recovery mnemonic: {e}")))
}

/// Outcome of a successful signup: the authenticated session plus the one-time recovery kit.
/// No `Debug` — it holds the unlocked trousseau and the recovery phrase (both secret).
pub struct SignupOutcome {
    pub account_id: String,
    /// The unlocked trousseau, for the caller to persist at rest.
    pub bundle: AccountKeyBundle,
    /// The 24-word BIP39 recovery phrase to display ONCE. Never persist or log it.
    ///
    /// SECURITY (intentionally a plain `String`, not `Zeroizing`): this phrase is a reversible
    /// encoding of the recovery entropy, which IS already wiped on drop (`generate_recovery_key`
    /// returns `Zeroizing<[u8; 32]>`, and the derived RWK is `Zeroizing` too). The phrase exists
    /// only to be shown to a human, so it necessarily crosses the FFI boundary as a plaintext
    /// JSON string and lands in Dart GC memory and on screen — neither of which Rust can zeroize.
    /// Wrapping this field would wipe one heap copy while ≥2 identical un-wiped copies (serde JSON,
    /// FFI marshalling, Dart string) coexist at display time, so it buys no real protection. An
    /// attacker who can read this process's heap already finds the full unlocked trousseau (held in
    /// RAM while signed in, A1). Display once, never persist (ADR-042 §8 / §14 L2).
    pub recovery_phrase: String,
}

#[derive(Debug)]
pub enum SignupError {
    /// The passphrase did not clear the strength floor (score < 4 or length < 12).
    WeakPassphrase(PassphraseStrength),
    /// An account already exists for this email (hub 409).
    AccountExists,
    /// Crypto failure generating or wrapping the trousseau.
    Crypto(String),
    /// Network or non-conflict hub error.
    Hub(String),
}

impl std::fmt::Display for SignupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WeakPassphrase(_) => write!(f, "Passphrase is too weak"),
            Self::AccountExists => write!(f, "An account already exists for this email"),
            Self::Crypto(e) => write!(f, "Crypto error: {e}"),
            Self::Hub(e) => write!(f, "Hub error: {e}"),
        }
    }
}

impl std::error::Error for SignupError {}

/// Create a new account on this (first) device. `device_entry` is this device's registry
/// entry (its random lane key + reused ADR-039 identity keys). On success the trousseau is
/// unlocked in RAM, `client` holds an authenticated session, and the first device registry
/// is published under the real `account_id`.
pub async fn signup(
    client: &mut AccountSyncClient,
    email: &str,
    passphrase: &SecretString,
    device_entry: DeviceEntry,
) -> Result<SignupOutcome, SignupError> {
    // 1. Hard gate: refuse a weak passphrase before any crypto or network (F7).
    let strength = check_passphrase(passphrase.expose_secret());
    if !strength.acceptable {
        return Err(SignupError::WeakPassphrase(strength));
    }

    // 2. Generate the random trousseau, account salt, and recovery key.
    let bundle = AccountKeyBundle::generate();
    let salt = generate_salt();
    let recovery_key = generate_recovery_key();
    let params = Argon2Params::default();

    // 3. Derive the Master Key (Argon2id 64 MiB) on the blocking pool so it never stalls the
    //    single-threaded FFI runtime (same pattern as account_enrollment / api/backup.rs).
    let passphrase_bytes: Zeroizing<Vec<u8>> =
        Zeroizing::new(passphrase.expose_secret().as_bytes().to_vec());
    let mk = tokio::task::spawn_blocking(move || {
        derive_master_key(passphrase_bytes.as_slice(), &salt, params)
    })
    .await
    .map_err(|e| SignupError::Crypto(format!("key derivation task failed: {e}")))?
    .map_err(|e| SignupError::Crypto(e.to_string()))?;

    // 4. Derive the wrapping keys and the auth verifier, then wrap the trousseau twice
    //    (passphrase + recovery copies — the recovery kit is mandatory, F7).
    let kwk = derive_kwk(&mk).map_err(|e| SignupError::Crypto(e.to_string()))?;
    let rwk = derive_recovery_wrapping_key(&recovery_key)
        .map_err(|e| SignupError::Crypto(e.to_string()))?;
    let auth_verifier =
        derive_auth_verifier(&mk).map_err(|e| SignupError::Crypto(e.to_string()))?;
    let wrapped_passphrase = wrap_bundle(&bundle, &kwk, WrapKind::Passphrase)
        .map_err(|e| SignupError::Crypto(e.to_string()))?;
    let wrapped_recovery = wrap_bundle(&bundle, &rwk, WrapKind::Recovery)
        .map_err(|e| SignupError::Crypto(e.to_string()))?;

    // 5. Sign the public descriptor with the account auth key (a joining device verifies it).
    let descriptor = account_descriptor_canonical(
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
    let descriptor_sig = bundle.sign_descriptor(&descriptor);

    // 6. Sign a placeholder first registry (the real account_id is not known until the
    //    signup response; it is overwritten in step 9 before any second device can join).
    let placeholder = DeviceRegistry {
        account_id: String::new(),
        registry_seq: 1,
        devices: vec![device_entry.clone()],
    };
    let placeholder_blob = placeholder
        .sign(&bundle.signing_key())
        .map_err(|e| SignupError::Crypto(e.to_string()))?;

    // 7. Register the account with the hub.
    let req = SignupRequest {
        email: email.to_string(),
        account_salt: URL_SAFE_NO_PAD.encode(salt),
        account_auth_pk: URL_SAFE_NO_PAD.encode(bundle.account_auth_pk()),
        descriptor_sig: URL_SAFE_NO_PAD.encode(descriptor_sig),
        auth_verifier_hash: auth_verifier_hash_hex(&auth_verifier),
        auth_method: AUTH_METHOD_PASSPHRASE.to_string(),
        aead_alg: AEAD_ALG_V1.to_string(),
        device_registry_blob: encode_blob_standard(&placeholder_blob),
        kdf_params: KdfParams {
            algo: KDF_ALGO_ARGON2ID.to_string(),
            version: ARGON2_VERSION_0X13,
            m: params.m_cost,
            t: params.t_cost,
            p: params.p_cost,
        },
        schema_version: ACCOUNT_SCHEMA_VERSION,
        wrapped_keys: vec![
            WrappedKeyDto {
                kind: WrapKind::Passphrase.wire_kind().to_string(),
                blob: encode_blob_standard(&wrapped_passphrase),
            },
            WrappedKeyDto {
                kind: WrapKind::Recovery.wire_kind().to_string(),
                blob: encode_blob_standard(&wrapped_recovery),
            },
        ],
    };
    let account_id = client.signup(&req).await.map_err(map_signup_err)?;

    // 8. Authenticate so the session is ready and we can publish the corrected registry.
    client
        .login(email, &bundle)
        .await
        .map_err(|e| SignupError::Hub(e.to_string()))?;

    // 9. Re-publish the registry signed with the REAL account_id (overwrites the placeholder).
    let registry = DeviceRegistry {
        account_id: account_id.clone(),
        registry_seq: 1,
        devices: vec![device_entry],
    };
    let signed = registry
        .sign(&bundle.signing_key())
        .map_err(|e| SignupError::Crypto(e.to_string()))?;
    client
        .post_registry(&encode_blob_standard(&signed))
        .await
        .map_err(|e| SignupError::Hub(e.to_string()))?;

    let recovery_phrase = recovery_phrase(&recovery_key)?;
    Ok(SignupOutcome {
        account_id,
        bundle,
        recovery_phrase,
    })
}

fn map_signup_err(e: AccountSyncError) -> SignupError {
    match e {
        AccountSyncError::Hub(409, _) => SignupError::AccountExists,
        other => SignupError::Hub(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::account_keys::{account_descriptor_canonical, verify_account_descriptor};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::VerifyingKey;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const STRONG: &str = "correct horse battery staple yonder";
    const EMAIL: &str = "new@example.org";

    fn device_entry() -> DeviceEntry {
        DeviceEntry {
            device_id: "lane-1".to_string(),
            ed25519_pk: [1u8; 32],
            x25519_pk: [2u8; 32],
            name: "first device".to_string(),
        }
    }

    #[test]
    fn weak_passphrase_is_rejected_short_or_low_score() {
        // Too short even if varied.
        assert!(!check_passphrase("aB3$xY").acceptable);
        // Long but trivially guessable -> low zxcvbn score.
        assert!(!check_passphrase("passwordpassword").acceptable);
        // A strong, long passphrase clears both floors.
        assert!(check_passphrase(STRONG).acceptable);
    }

    #[test]
    fn recovery_phrase_is_24_words() {
        let phrase = recovery_phrase(&[7u8; 32]).unwrap();
        assert_eq!(phrase.split_whitespace().count(), 24);
    }

    #[tokio::test]
    async fn signup_refuses_weak_passphrase_without_network() {
        // base_url points nowhere reachable: the gate must trip before any request.
        let mut client = AccountSyncClient::with_base_url("http://127.0.0.1:1");
        let err = signup(
            &mut client,
            EMAIL,
            &SecretString::new("short".into()),
            device_entry(),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, SignupError::WeakPassphrase(_)));
    }

    #[tokio::test]
    async fn signup_happy_path_creates_account_and_publishes_signed_registry() {
        let server = MockServer::start().await;

        // Capture the descriptor fields the client sent so we can verify the signature.
        Mock::given(method("POST"))
            .and(path("/api/account/signup"))
            .respond_with(|req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                // The descriptor signature must verify against the account auth pk the
                // client published (proves the canonical form is self-consistent).
                let pk_b = URL_SAFE_NO_PAD
                    .decode(body["account_auth_pk"].as_str().unwrap())
                    .unwrap();
                let salt_b = URL_SAFE_NO_PAD
                    .decode(body["account_salt"].as_str().unwrap())
                    .unwrap();
                let sig_b = URL_SAFE_NO_PAD
                    .decode(body["descriptor_sig"].as_str().unwrap())
                    .unwrap();
                let kdf = &body["kdf_params"];
                let canonical = account_descriptor_canonical(
                    &salt_b.clone().try_into().unwrap(),
                    &pk_b.clone().try_into().unwrap(),
                    kdf["algo"].as_str().unwrap(),
                    kdf["version"].as_u64().unwrap() as u32,
                    kdf["m"].as_u64().unwrap() as u32,
                    kdf["t"].as_u64().unwrap() as u32,
                    kdf["p"].as_u64().unwrap() as u32,
                    body["schema_version"].as_u64().unwrap() as u32,
                    body["auth_method"].as_str().unwrap(),
                    body["aead_alg"].as_str().unwrap(),
                );
                let vk = VerifyingKey::from_bytes(&pk_b.try_into().unwrap()).unwrap();
                assert!(
                    verify_account_descriptor(&vk, &canonical, &sig_b.try_into().unwrap()),
                    "descriptor_sig must verify against the published account auth pk"
                );
                // Two wrapped copies (passphrase + recovery) are mandatory (F7).
                assert_eq!(body["wrapped_keys"].as_array().unwrap().len(), 2);
                ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "account_id": "acct-new",
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "challenge": URL_SAFE_NO_PAD.encode([4u8; 32]),
                "expires_at": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "sess-new",
                "account_id": "acct-new",
                "descriptor": {
                    "account_salt": "c2FsdA",
                    "kdf_params": {"algo":"argon2id","version":19,"m":65536,"t":3,"p":1},
                    "schema_version": 1,
                    "auth_method": "passphrase",
                    "aead_alg": "AES-256-GCM",
                    "account_auth_pk": "cGs",
                    "descriptor_sig": "c2ln",
                },
            })))
            .mount(&server)
            .await;
        // The corrected registry must be published under the real account_id.
        Mock::given(method("POST"))
            .and(path("/api/account/registry"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "registry_seq": 2,
            })))
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let outcome = signup(
            &mut client,
            EMAIL,
            &SecretString::new(STRONG.into()),
            device_entry(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.account_id, "acct-new");
        assert!(client.is_authenticated());
        assert_eq!(outcome.recovery_phrase.split_whitespace().count(), 24);
        // The returned trousseau is the one whose auth key the descriptor was signed with.
        let _ = outcome.bundle.account_auth_pk();
    }

    #[tokio::test]
    async fn duplicate_email_maps_to_account_exists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/account/signup"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"error": "Account already exists"})),
            )
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        let err = signup(
            &mut client,
            EMAIL,
            &SecretString::new(STRONG.into()),
            device_entry(),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, SignupError::AccountExists));
    }
}
