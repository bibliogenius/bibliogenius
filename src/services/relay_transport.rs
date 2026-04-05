//! E2EE Relay Transport — send/receive encrypted messages via a relay hub.
//!
//! Parallel to `DirectTransport` (LAN, synchronous). RelayTransport is
//! fire-and-forget only: no request-response patterns via relay.
//!
//! See SECURITY_GUIDELINES.md §B8 (mailbox auth) and §B10 (polling jitter).

use std::sync::Arc;

use x25519_dalek::PublicKey as X25519PublicKey;

use crate::crypto::envelope::{ClearMessage, EncryptedEnvelope};
use crate::infrastructure::nonce_store::SqliteNonceStore;
use crate::services::crypto_service::CryptoService;
use crate::services::e2ee_transport::E2eeTransportError;

/// A message retrieved from a relay mailbox: either an encrypted envelope or a raw blob.
///
/// Raw blobs are messages that failed to parse as `EncryptedEnvelope`.
/// Used for non-E2EE relay messages such as connection requests.
pub enum RelayBlob {
    Encrypted(EncryptedEnvelope),
    Raw(i64, Vec<u8>),
}

/// Relay transport for sending/receiving messages via a hub mailbox.
///
/// The crypto service is optional: `poll()`, `ack()` and `deposit_raw()` work
/// without it (pure HTTP). Only `send()` and `deposit_response()` require crypto
/// for E2EE sealing. This allows the relay poller to process raw messages
/// (e.g. connection requests) even before the E2EE identity is initialized.
pub struct RelayTransport {
    crypto_service: Option<Arc<CryptoService<SqliteNonceStore>>>,
    http_client: reqwest::Client,
}

impl RelayTransport {
    pub fn new(crypto_service: Option<Arc<CryptoService<SqliteNonceStore>>>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();

        Self {
            crypto_service,
            http_client,
        }
    }

    /// Send an E2EE message via relay hub (fire-and-forget).
    ///
    /// 1. Seal the message (same as DirectTransport)
    /// 2. Serialize envelope to bytes
    /// 3. POST to relay: {relay_url}/api/relay/mailbox/{uuid}/messages
    ///
    /// Returns `Err(Crypto)` if the crypto service is not available.
    pub async fn send(
        &self,
        relay_url: &str,
        mailbox_uuid: &str,
        write_token: &str,
        peer_x25519_public: &X25519PublicKey,
        message: &ClearMessage,
    ) -> Result<(), E2eeTransportError> {
        // 1. Seal the message
        let crypto = self
            .crypto_service
            .as_ref()
            .ok_or_else(|| E2eeTransportError::Crypto("CryptoService not available".to_string()))?;
        let envelope = crypto
            .seal(peer_x25519_public, message)
            .map_err(|e| E2eeTransportError::Crypto(e.to_string()))?;

        // 2. Serialize envelope to bytes
        let blob =
            serde_json::to_vec(&envelope).map_err(|e| E2eeTransportError::Crypto(e.to_string()))?;

        // 3. POST to relay
        let url = format!(
            "{}/api/relay/mailbox/{}/messages",
            relay_url.trim_end_matches('/'),
            mailbox_uuid
        );

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {write_token}"))
            .header("Content-Type", "application/octet-stream")
            .body(blob)
            .send()
            .await
            .map_err(|e| E2eeTransportError::Network(format!("relay POST failed: {e}")))?;

        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            return Err(E2eeTransportError::PeerError(status, body));
        }

        tracing::info!(
            "E2EE Relay: Deposited '{}' to mailbox {}",
            message.message_type,
            mailbox_uuid
        );

        Ok(())
    }

    /// Poll relay for pending messages.
    ///
    /// Returns encrypted envelopes and raw blobs (non-E2EE messages such as
    /// connection requests). Raw blobs carry their message_id inside the enum
    /// variant so the caller can acknowledge them separately.
    pub async fn poll(
        &self,
        relay_url: &str,
        mailbox_uuid: &str,
        read_token: &str,
    ) -> Result<(Vec<(i64, EncryptedEnvelope)>, Vec<RelayBlob>), E2eeTransportError> {
        let url = format!(
            "{}/api/relay/mailbox/{}/messages",
            relay_url.trim_end_matches('/'),
            mailbox_uuid
        );

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {read_token}"))
            .send()
            .await
            .map_err(|e| E2eeTransportError::Network(format!("relay GET failed: {e}")))?;

        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            return Err(E2eeTransportError::PeerError(status, body));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| E2eeTransportError::Network(format!("invalid relay response: {e}")))?;

        let messages = body
            .get("messages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut envelopes = Vec::new();
        let mut raw_blobs = Vec::new();
        for msg in messages {
            let id = msg.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            let blob_b64 = msg.get("blob").and_then(|v| v.as_str()).unwrap_or("");

            use base64::Engine;
            let blob_bytes = match base64::engine::general_purpose::STANDARD.decode(blob_b64) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("Relay: Failed to decode blob {id}: {e}");
                    continue;
                }
            };

            match serde_json::from_slice::<EncryptedEnvelope>(&blob_bytes) {
                Ok(envelope) => envelopes.push((id, envelope)),
                Err(_) => {
                    // Not an E2EE envelope - keep as raw blob for caller to handle
                    // (e.g., connection_request messages from new peers)
                    raw_blobs.push(RelayBlob::Raw(id, blob_bytes));
                }
            }
        }

        Ok((envelopes, raw_blobs))
    }

    /// Deposit a raw (non-E2EE) message into a peer's relay mailbox.
    ///
    /// Used for connection requests where the recipient doesn't know the sender
    /// yet (no E2EE possible). The message is a JSON blob deposited as-is.
    pub async fn deposit_raw(
        &self,
        relay_url: &str,
        mailbox_uuid: &str,
        write_token: &str,
        body: &[u8],
    ) -> Result<(), E2eeTransportError> {
        let url = format!(
            "{}/api/relay/mailbox/{}/messages",
            relay_url.trim_end_matches('/'),
            mailbox_uuid
        );

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {write_token}"))
            .header("Content-Type", "application/octet-stream")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| E2eeTransportError::Network(format!("relay raw POST failed: {e}")))?;

        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(
                "Relay: deposit_raw to mailbox {mailbox_uuid} failed: HTTP {status} {body}"
            );
            return Err(E2eeTransportError::PeerError(status, body));
        }

        tracing::info!(
            "Relay: Deposited raw message to mailbox {mailbox_uuid} ({} bytes)",
            body.len()
        );
        Ok(())
    }

    /// Acknowledge (delete) a processed message from the relay.
    pub async fn ack(
        &self,
        relay_url: &str,
        mailbox_uuid: &str,
        read_token: &str,
        message_id: i64,
    ) -> Result<(), E2eeTransportError> {
        let url = format!(
            "{}/api/relay/mailbox/{}/messages/{}",
            relay_url.trim_end_matches('/'),
            mailbox_uuid,
            message_id
        );

        let response = self
            .http_client
            .delete(&url)
            .header("Authorization", format!("Bearer {read_token}"))
            .send()
            .await
            .map_err(|e| E2eeTransportError::Network(format!("relay DELETE failed: {e}")))?;

        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(
                "Relay: ack message {message_id} from mailbox {mailbox_uuid} failed: HTTP {status} {body}"
            );
            return Err(E2eeTransportError::PeerError(status, body));
        }

        tracing::debug!("Relay: Acked message {message_id} from mailbox {mailbox_uuid}");
        Ok(())
    }

    /// Deposit an encrypted response into a requester's mailbox (ADR-012 reply-to).
    ///
    /// Used when the relay poller processes a request that has `reply_to_mailbox`
    /// and `reply_to_write_token` fields. The response is encrypted and deposited
    /// directly into the requester's mailbox so they can pick it up on their next poll.
    ///
    /// Returns `Err(Crypto)` if the crypto service is not available.
    pub async fn deposit_response(
        &self,
        relay_url: &str,
        reply_to_mailbox: &str,
        reply_to_write_token: &str,
        peer_x25519_public: &X25519PublicKey,
        response_message: &ClearMessage,
    ) -> Result<(), E2eeTransportError> {
        // 1. Seal the response
        let crypto = self
            .crypto_service
            .as_ref()
            .ok_or_else(|| E2eeTransportError::Crypto("CryptoService not available".to_string()))?;
        let envelope = crypto
            .seal(peer_x25519_public, response_message)
            .map_err(|e| E2eeTransportError::Crypto(e.to_string()))?;

        // 2. Serialize envelope to bytes
        let blob =
            serde_json::to_vec(&envelope).map_err(|e| E2eeTransportError::Crypto(e.to_string()))?;

        // 3. POST to requester's mailbox
        let url = format!(
            "{}/api/relay/mailbox/{}/messages",
            relay_url.trim_end_matches('/'),
            reply_to_mailbox
        );

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {reply_to_write_token}"))
            .header("Content-Type", "application/octet-stream")
            .body(blob)
            .send()
            .await
            .map_err(|e| {
                E2eeTransportError::Network(format!("relay deposit_response POST failed: {e}"))
            })?;

        let status = response.status().as_u16();
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            return Err(E2eeTransportError::PeerError(status, body));
        }

        tracing::info!(
            "E2EE Relay: Deposited response '{}' to requester mailbox {}",
            response_message.message_type,
            reply_to_mailbox
        );

        Ok(())
    }

    /// Access the underlying crypto service (for opening received messages).
    /// Returns None if the transport was created without crypto.
    pub fn crypto_service(&self) -> Option<&Arc<CryptoService<SqliteNonceStore>>> {
        self.crypto_service.as_ref()
    }
}
