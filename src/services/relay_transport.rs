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

/// Relay transport for sending encrypted messages via a hub mailbox.
pub struct RelayTransport {
    crypto_service: Arc<CryptoService<SqliteNonceStore>>,
    http_client: reqwest::Client,
}

impl RelayTransport {
    pub fn new(crypto_service: Arc<CryptoService<SqliteNonceStore>>) -> Self {
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
    pub async fn send(
        &self,
        relay_url: &str,
        mailbox_uuid: &str,
        write_token: &str,
        peer_x25519_public: &X25519PublicKey,
        message: &ClearMessage,
    ) -> Result<(), E2eeTransportError> {
        // 1. Seal the message
        let envelope = self
            .crypto_service
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
    /// Returns Vec<(message_id, EncryptedEnvelope)> for each pending blob.
    pub async fn poll(
        &self,
        relay_url: &str,
        mailbox_uuid: &str,
        read_token: &str,
    ) -> Result<Vec<(i64, EncryptedEnvelope)>, E2eeTransportError> {
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

        let mut result = Vec::new();
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
                Ok(envelope) => result.push((id, envelope)),
                Err(e) => {
                    tracing::warn!("Relay: Failed to parse envelope {id}: {e}");
                }
            }
        }

        Ok(result)
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
            return Err(E2eeTransportError::PeerError(status, body));
        }

        Ok(())
    }

    /// Access the underlying crypto service (for opening received messages).
    pub fn crypto_service(&self) -> &Arc<CryptoService<SqliteNonceStore>> {
        &self.crypto_service
    }
}
