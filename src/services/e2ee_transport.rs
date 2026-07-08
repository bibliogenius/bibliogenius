//! E2EE Direct Transport — seal + POST encrypted messages to peers.
//!
//! Wraps `CryptoService.seal()` and HTTP POST to `{peer_url}/api/e2ee/message`.
//! Supports request-response patterns (e.g., search, sync) by opening the encrypted response.

use std::sync::Arc;

use serde_json::Value;
use x25519_dalek::PublicKey as X25519PublicKey;

use crate::crypto::envelope::{ClearMessage, EncryptedEnvelope};
use crate::infrastructure::nonce_store::SqliteNonceStore;
use crate::services::crypto_service::{CryptoService, PeerInfo};

/// Errors that can occur during E2EE transport.
#[derive(Debug)]
pub enum E2eeTransportError {
    /// Crypto operation failed (seal/open).
    Crypto(String),
    /// HTTP request to peer failed.
    Network(String),
    /// Peer returned an error status.
    PeerError(u16, String),
    /// Peer's relay write_token has been flagged invalid (ADR-032); the
    /// caller must not attempt a deposit until a fresh invitation is
    /// imported. Short-circuits the retry flood at the source.
    PeerInviteStale,
}

impl std::fmt::Display for E2eeTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crypto(msg) => write!(f, "crypto error: {msg}"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::PeerError(status, msg) => write!(f, "peer error ({status}): {msg}"),
            Self::PeerInviteStale => write!(f, "peer invitation is stale"),
        }
    }
}

impl E2eeTransportError {
    /// `true` when a direct send got an HTTP response that a real BiblioGenius
    /// peer can never produce for `POST /api/e2ee/message`: 404/405/501 mean
    /// the route or method is unknown to whatever answered. That happens when
    /// another service squats the peer's host:port (dev HTTP server, reverse
    /// proxy, captive portal) while the app is closed. The envelope was NOT
    /// processed by a peer, so a relay fallback is safe: no duplicate-delivery
    /// risk, unlike other 4xx/5xx codes the real endpoint returns after
    /// receiving the envelope (replay rejection, decrypt failure, ...).
    pub fn is_wrong_server_response(&self) -> bool {
        matches!(self, Self::PeerError(404 | 405 | 501, _))
    }
}

/// reqwest's `Display` only prints "error sending request for url (X)" and
/// hides the actual cause. Walk the source chain and tag the kind so logs
/// distinguish a timeout from a connection reset/refused — the difference
/// between "receiver too slow" and "receiver unreachable".
fn describe_reqwest_error(e: &reqwest::Error) -> String {
    use std::fmt::Write as _;

    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_request() {
        "request"
    } else {
        "other"
    };

    let mut detail = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let _ = write!(detail, ": {s}");
        src = s.source();
    }
    format!("[{kind}] {detail}")
}

/// Direct transport for sending encrypted messages to peers.
pub struct DirectTransport {
    crypto_service: Arc<CryptoService<SqliteNonceStore>>,
    http_client: reqwest::Client,
}

impl DirectTransport {
    /// Default per-request timeout. Kept short so the peer-sync path fails fast
    /// and falls back to relay. Interactive flows without a relay fallback
    /// (device sync) override this via [`Self::new_with_timeout`].
    const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    pub fn new(crypto_service: Arc<CryptoService<SqliteNonceStore>>) -> Self {
        Self::new_with_timeout(crypto_service, Self::DEFAULT_TIMEOUT)
    }

    /// Build a transport with a caller-chosen total request timeout (covers
    /// connect + send + awaiting the peer's response). Device sync needs a
    /// generous bound: on a never-synced device the receiver's round-trip
    /// (store remote ops, collect + enrich local ops, seal) easily exceeds the
    /// 3s peer-sync default, and that path has no relay fallback.
    pub fn new_with_timeout(
        crypto_service: Arc<CryptoService<SqliteNonceStore>>,
        timeout: std::time::Duration,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();

        Self {
            crypto_service,
            http_client,
        }
    }

    /// Send an encrypted message to a peer. Returns optional response ClearMessage.
    ///
    /// For fire-and-forget messages (loan_request, loan_confirmation), the response is None.
    /// For request-response patterns (search, sync), the peer returns a sealed response.
    pub async fn send(
        &self,
        peer_url: &str,
        peer_x25519_public: &X25519PublicKey,
        peer_info: &PeerInfo,
        message: &ClearMessage,
    ) -> Result<Option<ClearMessage>, E2eeTransportError> {
        // 1. Seal the message
        let envelope = self
            .crypto_service
            .seal(peer_x25519_public, message)
            .map_err(|e| E2eeTransportError::Crypto(e.to_string()))?;

        // 2. POST to peer's E2EE endpoint
        let url = format!("{peer_url}/api/e2ee/message");
        let response = self
            .http_client
            .post(&url)
            .header("X-E2EE", "true")
            .json(&envelope)
            .send()
            .await
            .map_err(|e| E2eeTransportError::Network(describe_reqwest_error(&e)))?;

        let status = response.status().as_u16();

        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            return Err(E2eeTransportError::PeerError(status, body));
        }

        // 3. Check if response is encrypted (request-response patterns like search, sync).
        //    Fire-and-forget handlers (loan_request, loan_confirmation, status_update)
        //    return plain JSON without the X-E2EE header — treat those as Ok(None).
        let is_encrypted_response = response
            .headers()
            .get("x-e2ee")
            .and_then(|v| v.to_str().ok())
            == Some("true");

        let body = response.bytes().await.unwrap_or_default();
        if body.is_empty() || !is_encrypted_response {
            return Ok(None);
        }

        // Parse sealed response envelope
        let response_envelope: EncryptedEnvelope = serde_json::from_slice(&body)
            .map_err(|e| E2eeTransportError::Crypto(format!("invalid response envelope: {e}")))?;

        // Open the response using the peer's info
        let known_peers = vec![peer_info.clone()];
        let (clear_response, _) = self
            .crypto_service
            .open(&response_envelope, &known_peers)
            .map_err(|e| E2eeTransportError::Crypto(format!("failed to open response: {e}")))?;

        tracing::info!(
            "E2EE: Received encrypted response type={}",
            clear_response.message_type
        );

        Ok(Some(clear_response))
    }

    /// Build a ClearMessage with standard fields.
    pub fn build_message(message_type: &str, payload: Value) -> ClearMessage {
        ClearMessage {
            message_type: message_type.to_string(),
            payload,
            timestamp: chrono::Utc::now().timestamp(),
            message_id: uuid::Uuid::new_v4().to_string(),
            correlation_id: None,
            reply_to_mailbox: None,
            reply_to_write_token: None,
        }
    }

    /// Access the underlying crypto service (for sealing responses).
    pub fn crypto_service(&self) -> &Arc<CryptoService<SqliteNonceStore>> {
        &self.crypto_service
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_server_statuses_are_fallback_eligible() {
        for status in [404u16, 405, 501] {
            assert!(
                E2eeTransportError::PeerError(status, String::new()).is_wrong_server_response(),
                "{status} must be treated as a squatted port"
            );
        }
    }

    #[test]
    fn real_peer_errors_are_not_fallback_eligible() {
        // Codes the real /api/e2ee/message endpoint can return after having
        // processed the envelope: relaying those could duplicate delivery.
        for status in [400u16, 401, 403, 409, 500, 502, 503] {
            assert!(
                !E2eeTransportError::PeerError(status, String::new()).is_wrong_server_response(),
                "{status} may come from a real peer, no relay fallback"
            );
        }
        assert!(!E2eeTransportError::Crypto("x".into()).is_wrong_server_response());
        assert!(!E2eeTransportError::Network("x".into()).is_wrong_server_response());
        assert!(!E2eeTransportError::PeerInviteStale.is_wrong_server_response());
    }
}
