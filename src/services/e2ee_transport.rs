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

/// Direct transport for sending encrypted messages to peers.
pub struct DirectTransport {
    crypto_service: Arc<CryptoService<SqliteNonceStore>>,
    http_client: reqwest::Client,
}

impl DirectTransport {
    pub fn new(crypto_service: Arc<CryptoService<SqliteNonceStore>>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
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
            .map_err(|e| E2eeTransportError::Network(e.to_string()))?;

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
