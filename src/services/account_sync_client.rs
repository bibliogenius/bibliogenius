//! Account sync hub client.
//!
//! Rust client for the ADR-043 blind lane store and its account lifecycle/auth
//! endpoints (`AccountController` / `AccountSyncController` on the hub). The hub never
//! holds a decrypting secret: it stores public auth material, opaque wrapped key
//! bundles, the signed device registry, and ciphertext lanes keyed by opaque ids.
//!
//! This module owns the WIRE layer only — request/response DTOs, the two auth
//! crypto challenge-responses, and the HTTP calls. The local merge/cursor state and
//! the bundle-at-rest persistence are deliberately NOT here: the pull cursor is read
//! and written by the local sync loop, and the bundle is persisted by the account FFI
//! service. Keeping this module stateless (the session token aside) makes it
//! fully testable against a mock hub without touching the database.
//!
//! Crypto interop, verified against `AccountAuthService.php`:
//! - **login** = Ed25519 signature over the RAW DECODED challenge bytes
//!   (`sodium_crypto_sign_verify_detached`); `pk`/`sig`/`challenge` are base64url.
//! - **keybundle download** = `HMAC-SHA256(key = auth_verifier_hash string,
//!   msg = challenge string)` rendered as lowercase HEX (PHP `hash_hmac('sha256', …)`,
//!   NOT libsodium `crypto_auth`, which would be HMAC-SHA512-256).
//! - **session** = opaque 256-bit bearer token (`Authorization: Bearer …`), no JWT.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::Signer;
use hmac::{Hmac, Mac};
use reqwest::{Client, RequestBuilder};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use zeroize::Zeroizing;

use crate::crypto::account_keys::AccountKeyBundle;

/// Challenge purposes accepted by the hub (`AccountAuthChallenge::PURPOSES`).
pub const PURPOSE_LOGIN: &str = "login";
pub const PURPOSE_KEYBUNDLE: &str = "keybundle";

// ---------------------------------------------------------------------------
// Error type (mirrors the HubDirectoryError shape used elsewhere)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AccountSyncError {
    /// Network or transport failure.
    Network(String),
    /// Hub returned a non-2xx status (code, body).
    Hub(u16, String),
    /// No session token held but a protected endpoint was called.
    NotAuthenticated,
    /// Local configuration or environment issue (e.g. HUB_URL unset).
    Config(String),
    /// Crypto failure building an auth response.
    Crypto(String),
}

impl std::fmt::Display for AccountSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "Network error: {e}"),
            Self::Hub(code, msg) => write!(f, "Hub error {code}: {msg}"),
            Self::NotAuthenticated => write!(f, "Not authenticated with account hub"),
            Self::Config(e) => write!(f, "Configuration error: {e}"),
            Self::Crypto(e) => write!(f, "Crypto error: {e}"),
        }
    }
}

impl std::error::Error for AccountSyncError {}

impl From<reqwest::Error> for AccountSyncError {
    fn from(e: reqwest::Error) -> Self {
        Self::Network(e.to_string())
    }
}

type Result<T> = std::result::Result<T, AccountSyncError>;

// ---------------------------------------------------------------------------
// Data transfer objects (hub API contract)
// ---------------------------------------------------------------------------

/// Argon2id parameters stored with the account (public). Mirrors what `bootstrap`
/// and the login `descriptor` return so every device derives the same Master Key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct KdfParams {
    pub algo: String,
    pub version: u32,
    pub m: u32,
    pub t: u32,
    pub p: u32,
}

/// One wrapped key copy (passphrase / recovery / escrow), blob is standard base64.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct WrappedKeyDto {
    pub kind: String,
    pub blob: String,
}

/// Public account descriptor returned by `bootstrap` and inside the login response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AccountDescriptor {
    pub account_salt: String,
    pub kdf_params: KdfParams,
    pub schema_version: u32,
    pub auth_method: String,
    pub aead_alg: String,
    pub account_auth_pk: String,
    pub descriptor_sig: String,
}

/// Fields for `POST /signup`. The caller (the account FFI service) assembles these
/// from a freshly generated bundle + the chosen passphrase / recovery kit.
#[derive(Debug, Clone, Serialize)]
pub struct SignupRequest {
    pub email: String,
    /// base64url(32B) account salt.
    pub account_salt: String,
    /// base64url(32B) Ed25519 account auth public key.
    pub account_auth_pk: String,
    /// base64url(64B) signature over the account descriptor.
    pub descriptor_sig: String,
    /// Hex SHA-256 of the AuthVerifier (the HMAC key for the keybundle gate).
    pub auth_verifier_hash: String,
    pub auth_method: String,
    pub aead_alg: String,
    /// Standard base64 of the opaque signed device registry.
    pub device_registry_blob: String,
    pub kdf_params: KdfParams,
    pub schema_version: u32,
    pub wrapped_keys: Vec<WrappedKeyDto>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SignupResponse {
    account_id: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ChallengeResponse {
    challenge: String,
    #[allow(dead_code)]
    expires_at: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct LoginResponse {
    token: String,
    account_id: String,
    descriptor: AccountDescriptor,
}

/// Result of a successful login: the session is stored on the client; the caller
/// gets the account id and descriptor it needs to derive keys.
#[derive(Debug, Clone)]
pub struct LoginOutcome {
    pub account_id: String,
    pub descriptor: AccountDescriptor,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct KeybundleResponse {
    wrapped_keys: Vec<WrappedKeyDto>,
}

/// A lane to push: the entity ciphertext (or a tombstone with `blob = None`).
#[derive(Debug, Clone, Serialize)]
pub struct LanePush {
    /// base64url opaque id (`^[A-Za-z0-9_-]{1,64}$`).
    pub opaque_id: String,
    pub deleted: bool,
    pub size_bucket: i64,
    /// Standard base64 ciphertext; `None` only for tombstones.
    pub blob: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PushRequest<'a> {
    device_id: &'a str,
    lanes: &'a [LanePush],
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PushResponse {
    pub accepted: u32,
    pub high_change_seq: i64,
}

/// A lane returned by `pull` (another device's lane after the cursor).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LanePull {
    pub opaque_id: String,
    pub device_id: String,
    pub change_seq: i64,
    pub deleted: bool,
    pub size_bucket: i64,
    /// Standard base64 ciphertext; `None` for tombstones with the blob GC'd.
    pub blob: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PullResponse {
    pub lanes: Vec<LanePull>,
    pub next_cursor: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RegistryResponse {
    /// Standard base64 of the signed registry, or `None` if never published.
    pub blob: Option<String>,
    pub registry_seq: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RegistrySeqResponse {
    registry_seq: i64,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct DeletedResponse {
    deleted: i64,
}

// ---------------------------------------------------------------------------
// Auth crypto (pure, testable — no network)
// ---------------------------------------------------------------------------

/// Hex SHA-256 of the AuthVerifier — the value stored on the hub and used as the
/// HMAC key for the keybundle gate. Sent verbatim at signup as `auth_verifier_hash`.
pub fn auth_verifier_hash_hex(auth_verifier: &[u8; 32]) -> String {
    let digest = Sha256::digest(auth_verifier);
    hex::encode(digest)
}

/// Ed25519 login response: sign the RAW DECODED challenge bytes, return base64url.
fn build_login_signature(bundle: &AccountKeyBundle, challenge_b64url: &str) -> Result<String> {
    let challenge = URL_SAFE_NO_PAD
        .decode(challenge_b64url)
        .map_err(|e| AccountSyncError::Crypto(format!("bad challenge: {e}")))?;
    let signature = bundle.signing_key().sign(&challenge);
    Ok(URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

/// Keybundle download MAC: `HMAC-SHA256(key = auth_verifier_hash, msg = challenge)`
/// over the STRING bytes, lowercase hex — matches PHP `hash_hmac('sha256', …)`.
fn build_keybundle_mac(auth_verifier_hash: &str, challenge_b64url: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(auth_verifier_hash.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(challenge_b64url.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Credentials kept so the client can mint itself a fresh session token.
///
/// The hub caps a session at 30 minutes (`AccountAuthService::SESSION_TOKEN_TTL_SECONDS`)
/// and does not slide the TTL on use, while a desktop process stays alive for days.
/// Without this the token minted at launch simply ages out and every later call 401s
/// until the app is restarted.
struct ReauthCredentials {
    email: String,
    /// Shared with the caller's session rather than cloned: the trousseau is secret
    /// material (A1) and must exist once in RAM, zeroized on the last drop.
    bundle: Arc<AccountKeyBundle>,
}

pub struct AccountSyncClient {
    http: Client,
    base_url: String,
    /// Opaque bearer session token, zeroized on drop (A1: tokens are secrets).
    /// Behind a lock because the bearer-protected calls take `&self` (the `LaneTransport`
    /// seam is a shared-reference trait) yet must be able to replace an expired token.
    token: RwLock<Option<SecretString>>,
    /// Set by [`AccountSyncClient::enable_auto_reauth`]; `None` disables renewal, which
    /// is the right default for the one-shot signup/enrollment flows.
    reauth: Option<ReauthCredentials>,
    /// Serializes renewals so a burst of concurrent 401s mints one token, not one each.
    renew_lock: tokio::sync::Mutex<()>,
}

impl AccountSyncClient {
    /// Build a client pointed at the configured hub (`HUB_URL`).
    pub fn new() -> Result<Self> {
        Ok(Self::with_base_url(hub_base_url()?))
    }

    /// Build a client against an explicit base URL (no trailing slash). Used by tests.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .user_agent("BiblioGenius/1.0")
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: RwLock::new(None),
            reauth: None,
            renew_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Arm automatic session renewal for a signed-in account: when the hub expires the
    /// session token mid-run, the next protected call re-logs in from `bundle` and
    /// replays itself instead of surfacing a 401 (see [`ReauthCredentials`]).
    pub fn enable_auto_reauth(&mut self, email: impl Into<String>, bundle: Arc<AccountKeyBundle>) {
        self.reauth = Some(ReauthCredentials {
            email: email.into(),
            bundle,
        });
    }

    /// Whether a session token is currently held.
    pub fn is_authenticated(&self) -> bool {
        self.token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Copy the session token out of the lock, so no guard is held across an await point.
    /// `Zeroizing` because this copy is the token in the clear (A1: tokens are secrets).
    fn current_token(&self) -> Option<Zeroizing<String>> {
        self.token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|t| Zeroizing::new(t.expose_secret().to_string()))
    }

    fn set_token(&self, token: String) {
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = Some(SecretString::new(token));
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    // --- account lifecycle / auth ---

    /// `POST /signup` — create the account. Returns the new opaque account id.
    pub async fn signup(&self, req: &SignupRequest) -> Result<String> {
        let resp: SignupResponse = self
            .send_json(self.http.post(self.url("/api/account/signup")).json(req))
            .await?;
        Ok(resp.account_id)
    }

    /// `GET /bootstrap` — public material a fresh device needs (path A).
    pub async fn bootstrap(&self, email: &str) -> Result<AccountDescriptor> {
        self.send_json(
            self.http
                .get(self.url("/api/account/bootstrap"))
                .query(&[("email", email)]),
        )
        .await
    }

    /// `POST /challenge` — issue a one-time nonce for `purpose`.
    async fn request_challenge(&self, email: &str, purpose: &str) -> Result<String> {
        let resp: ChallengeResponse = self
            .send_json(
                self.http
                    .post(self.url("/api/account/challenge"))
                    .json(&serde_json::json!({ "email": email, "purpose": purpose })),
            )
            .await?;
        Ok(resp.challenge)
    }

    /// Full login: fetch a challenge, sign it with the bundle, exchange for a token.
    /// On success the session token is stored for subsequent protected calls.
    pub async fn login(&mut self, email: &str, bundle: &AccountKeyBundle) -> Result<LoginOutcome> {
        self.authenticate(email, bundle).await
    }

    /// The login round-trip itself, shared by the explicit [`AccountSyncClient::login`] and
    /// by the automatic renewal a 401 triggers (hence `&self`).
    async fn authenticate(&self, email: &str, bundle: &AccountKeyBundle) -> Result<LoginOutcome> {
        let challenge = self.request_challenge(email, PURPOSE_LOGIN).await?;
        let signature = build_login_signature(bundle, &challenge)?;
        let resp: LoginResponse =
            self.send_json(self.http.post(self.url("/api/account/login")).json(
                &serde_json::json!({
                    "email": email,
                    "challenge": challenge,
                    "signature": signature,
                }),
            ))
            .await?;
        self.set_token(resp.token);
        Ok(LoginOutcome {
            account_id: resp.account_id,
            descriptor: resp.descriptor,
        })
    }

    /// Mint a fresh session token from the stored trousseau after the hub rejected
    /// `expired`. Returns `false` when auto-reauth is not armed, so the caller surfaces
    /// the original 401 rather than a misleading error.
    ///
    /// Serialized on `renew_lock`: concurrent callers that lose the race find the token
    /// another task already installed and skip the round-trip.
    async fn renew_session(&self, expired: &str) -> Result<bool> {
        let Some(creds) = self.reauth.as_ref() else {
            return Ok(false);
        };
        let _guard = self.renew_lock.lock().await;
        if matches!(self.current_token(), Some(current) if current.as_str() != expired) {
            return Ok(true);
        }
        tracing::info!("account hub session expired; re-authenticating");
        self.authenticate(&creds.email, &creds.bundle).await?;
        Ok(true)
    }

    /// Download wrapped key copies (path A bootstrap / recovery). `auth_verifier_hash`
    /// is the same hex string sent at signup; `kinds` defaults to `["passphrase"]`.
    pub async fn download_keybundle(
        &self,
        email: &str,
        auth_verifier_hash: &str,
        kinds: &[&str],
    ) -> Result<Vec<WrappedKeyDto>> {
        let challenge = self.request_challenge(email, PURPOSE_KEYBUNDLE).await?;
        let mac = build_keybundle_mac(auth_verifier_hash, &challenge);
        let resp: KeybundleResponse = self
            .send_json(self.http.post(self.url("/api/account/keybundle")).json(
                &serde_json::json!({
                    "email": email,
                    "challenge": challenge,
                    "mac": mac,
                    "kinds": kinds,
                }),
            ))
            .await?;
        Ok(resp.wrapped_keys)
    }

    // --- sync (bearer-protected) ---

    /// `POST /push` — blind overwrite-in-place of this device's lanes.
    pub async fn push(&self, device_id: &str, lanes: &[LanePush]) -> Result<PushResponse> {
        let body = PushRequest { device_id, lanes };
        self.send_authed(|| self.http.post(self.url("/api/account/push")).json(&body))
            .await
    }

    /// `GET /pull` — delta of OTHER devices' lanes after `cursor`. Pass this device's
    /// id so the hub excludes its own lanes; `cursor = 0` is a full bootstrap.
    pub async fn pull(&self, device_id: &str, cursor: i64, limit: u32) -> Result<PullResponse> {
        self.send_authed(|| {
            self.http.get(self.url("/api/account/pull")).query(&[
                ("cursor", cursor.to_string()),
                ("limit", limit.to_string()),
                ("device_id", device_id.to_string()),
            ])
        })
        .await
    }

    /// `GET /registry` — the opaque signed device registry.
    pub async fn get_registry(&self) -> Result<RegistryResponse> {
        self.send_authed(|| self.http.get(self.url("/api/account/registry")))
            .await
    }

    /// `POST /registry` — publish a new signed registry (standard base64 blob).
    pub async fn post_registry(&self, blob_b64: &str) -> Result<i64> {
        let resp: RegistrySeqResponse = self
            .send_authed(|| {
                self.http
                    .post(self.url("/api/account/registry"))
                    .json(&serde_json::json!({ "blob": blob_b64 }))
            })
            .await?;
        Ok(resp.registry_seq)
    }

    /// `DELETE /lanes?device_id=` — client-driven orphan-lane GC. Returns rows deleted.
    pub async fn delete_lanes(&self, device_id: &str) -> Result<i64> {
        let resp: DeletedResponse = self
            .send_authed(|| {
                self.http
                    .delete(self.url("/api/account/lanes"))
                    .query(&[("device_id", device_id)])
            })
            .await?;
        Ok(resp.deleted)
    }

    /// `DELETE /api/account` — RGPD purge of the whole account.
    pub async fn delete_account(&self) -> Result<()> {
        let _: serde_json::Value = self
            .send_authed(|| self.http.delete(self.url("/api/account")))
            .await?;
        Ok(())
    }

    // --- transport helpers ---

    /// Send a bearer-protected request, renewing an expired session once and replaying it.
    ///
    /// `build` rebuilds the request rather than taking one, because a `RequestBuilder` is
    /// consumed on send and the retry needs a second, identical one carrying the new token.
    /// A 401 that we cannot renew away (no credentials armed, or the re-login itself
    /// fails) is surfaced to the caller.
    async fn send_authed<T, F>(&self, build: F) -> Result<T>
    where
        T: DeserializeOwned,
        F: Fn() -> RequestBuilder,
    {
        let token = self
            .current_token()
            .ok_or(AccountSyncError::NotAuthenticated)?;
        match self.send_json(build().bearer_auth(token.as_str())).await {
            Err(AccountSyncError::Hub(401, body)) => {
                if !self.renew_session(&token).await? {
                    return Err(AccountSyncError::Hub(401, body));
                }
                let token = self
                    .current_token()
                    .ok_or(AccountSyncError::NotAuthenticated)?;
                self.send_json(build().bearer_auth(token.as_str())).await
            }
            other => other,
        }
    }

    /// Send a request, map non-2xx to `Hub(status, body)`, and deserialize the body.
    async fn send_json<T: DeserializeOwned>(&self, req: RequestBuilder) -> Result<T> {
        let resp = req.send().await?;
        let status = resp.status();
        if status.is_success() {
            resp.json::<T>().await.map_err(AccountSyncError::from)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(AccountSyncError::Hub(status.as_u16(), body))
        }
    }
}

/// Resolve the hub base URL from `HUB_URL` (kept in sync with the relay config),
/// trimming a trailing slash. Same source of truth as `HubDirectoryService`.
fn hub_base_url() -> Result<String> {
    std::env::var("HUB_URL")
        .map(|u| u.trim_end_matches('/').to_string())
        .map_err(|_| AccountSyncError::Config("HUB_URL environment variable not set".to_string()))
}

/// Standard-base64 encode (lane / registry / wrapped-key blobs the hub decodes with
/// `base64_decode($v, true)`).
pub fn encode_blob_standard(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode a standard-base64 blob returned by the hub.
pub fn decode_blob_standard(value: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(value)
        .map_err(|e| AccountSyncError::Crypto(format!("bad base64 blob: {e}")))
}

/// base64url(no-pad) encode for opaque ids / keys / signatures the hub decodes with
/// `strtr` + `base64_decode` (salt, pk, sig, opaque_id, device_id).
pub fn encode_b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // --- pure auth crypto ---

    #[test]
    fn login_signature_verifies_with_account_key() {
        let bundle = AccountKeyBundle::generate();
        // The hub generates challenges as base64url(no-pad) of 32 random bytes.
        let raw_challenge = [9u8; 32];
        let challenge_b64 = URL_SAFE_NO_PAD.encode(raw_challenge);

        let sig_b64 = build_login_signature(&bundle, &challenge_b64).unwrap();
        let sig_bytes = URL_SAFE_NO_PAD.decode(&sig_b64).unwrap();
        let signature = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();

        // The hub verifies the signature over the RAW decoded challenge bytes.
        assert!(
            bundle
                .verifying_key()
                .verify(&raw_challenge, &signature)
                .is_ok()
        );
    }

    #[test]
    fn keybundle_mac_matches_hmac_sha256_rfc4231_vector() {
        // RFC 4231 test case 2 locks our HMAC-SHA256 to the same bytes PHP's
        // hash_hmac('sha256', data, key) produces (key="Jefe", data="what do ya...").
        let mac = build_keybundle_mac("Jefe", "what do ya want for nothing?");
        assert_eq!(
            mac,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn auth_verifier_hash_is_sha256_hex() {
        // SHA-256 of 32 zero bytes, locked so client and hub agree on the HMAC key.
        let hash = auth_verifier_hash_hex(&[0u8; 32]);
        assert_eq!(
            hash,
            "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925"
        );
    }

    // --- HTTP against a mock hub ---

    fn kdf_params() -> KdfParams {
        KdfParams {
            algo: "argon2id".to_string(),
            version: 19,
            m: 65536,
            t: 3,
            p: 1,
        }
    }

    #[tokio::test]
    async fn signup_returns_account_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/account/signup"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({"account_id": "acct-xyz"})),
            )
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        let req = SignupRequest {
            email: "a@b.co".into(),
            account_salt: "c2FsdA".into(),
            account_auth_pk: "cGs".into(),
            descriptor_sig: "c2ln".into(),
            auth_verifier_hash: "deadbeef".into(),
            auth_method: "passphrase".into(),
            aead_alg: "AES-256-GCM".into(),
            device_registry_blob: "cmVn".into(),
            kdf_params: kdf_params(),
            schema_version: 1,
            wrapped_keys: vec![WrappedKeyDto {
                kind: "passphrase".into(),
                blob: "d3JhcA==".into(),
            }],
        };
        assert_eq!(client.signup(&req).await.unwrap(), "acct-xyz");
    }

    #[tokio::test]
    async fn login_flow_stores_token_and_authenticates_push() {
        let server = MockServer::start().await;
        let bundle = AccountKeyBundle::generate();
        let challenge_b64 = URL_SAFE_NO_PAD.encode([3u8; 32]);

        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "challenge": challenge_b64,
                "expires_at": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "sess-123",
                "account_id": "acct-1",
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
        // Push must carry the bearer token issued by login.
        Mock::given(method("POST"))
            .and(path("/api/account/push"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer sess-123",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"accepted": 1, "high_change_seq": 42})),
            )
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        assert!(!client.is_authenticated());
        let outcome = client.login("a@b.co", &bundle).await.unwrap();
        assert_eq!(outcome.account_id, "acct-1");
        assert!(client.is_authenticated());

        let lanes = vec![LanePush {
            opaque_id: "oid1".into(),
            deleted: false,
            size_bucket: 1024,
            blob: Some("Y2lwaGVy".into()),
        }];
        let resp = client.push("dev-1", &lanes).await.unwrap();
        assert_eq!(resp.high_change_seq, 42);
    }

    #[tokio::test]
    async fn pull_sends_device_id_and_parses_lanes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/account/pull"))
            .and(query_param("device_id", "dev-self"))
            .and(query_param("cursor", "7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "lanes": [{
                    "opaque_id": "oidA",
                    "device_id": "dev-other",
                    "change_seq": 9,
                    "deleted": false,
                    "size_bucket": 1024,
                    "blob": "Y2lwaGVy",
                }],
                "next_cursor": 9,
            })))
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        client.set_token("t".to_string());
        let resp = client.pull("dev-self", 7, 200).await.unwrap();
        assert_eq!(resp.next_cursor, 9);
        assert_eq!(resp.lanes.len(), 1);
        assert_eq!(resp.lanes[0].device_id, "dev-other");
    }

    #[tokio::test]
    async fn push_without_token_is_not_authenticated() {
        let client = AccountSyncClient::with_base_url("http://127.0.0.1:1");
        let err = client.push("dev", &[]).await.unwrap_err();
        assert!(matches!(err, AccountSyncError::NotAuthenticated));
    }

    /// Mount the challenge + login pair a renewal needs, issuing `token`. `expected_logins`
    /// is verified when the mock server drops.
    async fn mount_login(server: &MockServer, token: &str, expected_logins: u64) {
        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "challenge": URL_SAFE_NO_PAD.encode([7u8; 32]),
                "expires_at": "2026-01-01T00:00:00Z",
            })))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": token,
                "account_id": "acct-1",
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
            .expect(expected_logins)
            .mount(server)
            .await;
    }

    /// A hub session expires after 30 minutes and the hub does not slide the TTL, so a
    /// long-lived process (a desktop app left open) eventually pushes with a dead token.
    /// The client must re-login and replay the call instead of surfacing the 401.
    #[tokio::test]
    async fn expired_session_is_renewed_and_the_call_retried() {
        let server = MockServer::start().await;
        mount_login(&server, "fresh", 1).await;
        // The hub rejects the aged-out token...
        Mock::given(method("POST"))
            .and(path("/api/account/push"))
            .and(wiremock::matchers::header("authorization", "Bearer stale"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error":"Unauthorized"})),
            )
            .mount(&server)
            .await;
        // ...and accepts the one minted by the renewal.
        Mock::given(method("POST"))
            .and(path("/api/account/push"))
            .and(wiremock::matchers::header("authorization", "Bearer fresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"accepted": 1, "high_change_seq": 42})),
            )
            .mount(&server)
            .await;

        let mut client = AccountSyncClient::with_base_url(server.uri());
        client.enable_auto_reauth("a@b.co", Arc::new(AccountKeyBundle::generate()));
        client.set_token("stale".to_string());

        let resp = client.push("dev-1", &[]).await.unwrap();
        assert_eq!(resp.high_change_seq, 42);
        // The renewed token is the one kept for subsequent calls.
        assert_eq!(client.current_token().unwrap().as_str(), "fresh");
    }

    /// Without credentials armed (the one-shot signup/enrollment clients) there is nothing
    /// to renew with: the 401 must reach the caller rather than be retried blindly.
    #[tokio::test]
    async fn expired_session_without_auto_reauth_surfaces_the_401() {
        let server = MockServer::start().await;
        mount_login(&server, "fresh", 0).await;
        Mock::given(method("POST"))
            .and(path("/api/account/push"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error":"Unauthorized"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        client.set_token("stale".to_string());

        match client.push("dev-1", &[]).await.unwrap_err() {
            AccountSyncError::Hub(code, _) => assert_eq!(code, 401),
            other => panic!("expected Hub 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_2xx_maps_to_hub_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/account/bootstrap"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"error":"Account not found"})),
            )
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        match client.bootstrap("missing@b.co").await.unwrap_err() {
            AccountSyncError::Hub(code, _) => assert_eq!(code, 404),
            other => panic!("expected Hub error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_roundtrip_dtos() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/account/registry"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"blob": "cmVn", "registry_seq": 5})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/account/registry"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"registry_seq": 6})),
            )
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        client.set_token("t".to_string());
        let got = client.get_registry().await.unwrap();
        assert_eq!(got.registry_seq, 5);
        assert_eq!(got.blob.as_deref(), Some("cmVn"));
        assert_eq!(client.post_registry("bmV3").await.unwrap(), 6);
    }

    #[tokio::test]
    async fn download_keybundle_sends_mac_and_parses_wrapped_keys() {
        let server = MockServer::start().await;
        let challenge_b64 = URL_SAFE_NO_PAD.encode([5u8; 32]);
        let auth_verifier_hash = "deadbeefcafe";
        let expected_mac = build_keybundle_mac(auth_verifier_hash, &challenge_b64);

        Mock::given(method("POST"))
            .and(path("/api/account/challenge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "challenge": challenge_b64,
                "expires_at": "2026-01-01T00:00:00Z",
            })))
            .mount(&server)
            .await;
        // The keybundle gate must receive the hex MAC we computed for this challenge.
        Mock::given(method("POST"))
            .and(path("/api/account/keybundle"))
            .and(body_partial_json(
                serde_json::json!({ "mac": expected_mac }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "wrapped_keys": [{ "kind": "passphrase", "blob": "d3JhcA==" }],
            })))
            .mount(&server)
            .await;

        let client = AccountSyncClient::with_base_url(server.uri());
        let keys = client
            .download_keybundle("a@b.co", auth_verifier_hash, &["passphrase"])
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].kind, "passphrase");
        assert_eq!(keys[0].blob, "d3JhcA==");
    }
}
