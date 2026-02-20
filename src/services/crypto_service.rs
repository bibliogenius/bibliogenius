use ed25519_dalek::{Signer, Verifier};
use x25519_dalek::PublicKey as X25519PublicKey;

use crate::crypto::encryption::{decrypt_aes_gcm, encrypt_aes_gcm, pad_to_block, unpad};
use crate::crypto::envelope::{ClearMessage, EncryptedEnvelope, SignedPayload};
use crate::crypto::errors::CryptoError;
use crate::crypto::identity::NodeIdentity;
use crate::crypto::key_exchange::{
    compute_sender_hint, receiver_key_exchange, sender_key_exchange,
};

/// Padding block size for anti-CRIME mitigation (B9).
const PAD_BLOCK_SIZE: usize = 256;

/// Maximum allowed timestamp drift in seconds (B4).
const MAX_TIMESTAMP_DRIFT_SECS: i64 = 300; // ±5 minutes

/// Trait for anti-replay nonce storage (B4).
///
/// Implementations can be in-memory (for tests) or SQLite (production).
pub trait NonceStore: Send + Sync {
    /// Check if a nonce has been seen before.
    fn exists(&self, nonce: &[u8; 12]) -> Result<bool, CryptoError>;
    /// Record a nonce as seen.
    fn insert(&self, nonce: &[u8; 12]) -> Result<(), CryptoError>;
}

/// Peer info needed for sender identification.
#[derive(Clone)]
pub struct PeerInfo {
    /// Ed25519 verifying key (to check signatures after decryption).
    pub verifying_key: ed25519_dalek::VerifyingKey,
    /// X25519 static public key (for sender_hint matching).
    pub x25519_public: X25519PublicKey,
}

/// High-level crypto service implementing the full seal/open pipeline.
///
/// See SECURITY_GUIDELINES.md Part C for the complete pipeline specification.
pub struct CryptoService<N: NonceStore> {
    identity: NodeIdentity,
    nonce_store: N,
}

impl<N: NonceStore> CryptoService<N> {
    pub fn new(identity: NodeIdentity, nonce_store: N) -> Self {
        Self {
            identity,
            nonce_store,
        }
    }

    /// Seal a message for a specific peer (full pipeline per Part C).
    ///
    /// Pipeline: JSON → compress → sign → pad → ephemeral DH → HKDF → AES-GCM encrypt
    pub fn seal(
        &self,
        peer_static_public: &X25519PublicKey,
        message: &ClearMessage,
    ) -> Result<EncryptedEnvelope, CryptoError> {
        // 1. Serialize to JSON
        let json =
            serde_json::to_vec(message).map_err(|e| CryptoError::Serialization(e.to_string()))?;

        // 2. Compress with zstd (level 3)
        let compressed = zstd::encode_all(json.as_slice(), 3)
            .map_err(|e| CryptoError::Compression(e.to_string()))?;

        // 3. Sign the compressed data BEFORE encryption (B1: sign-then-encrypt)
        let signature = self.identity.signing_key().sign(&compressed);

        // 4. Assemble signed payload
        let signed_payload = SignedPayload {
            data: compressed,
            signature: signature.to_bytes().to_vec(),
        };

        // 5. Serialize with MessagePack (compact)
        let signed_bytes = rmp_serde::to_vec(&signed_payload)
            .map_err(|e| CryptoError::Serialization(e.to_string()))?;

        // 6. Pad to block boundary (B9: anti-CRIME)
        let padded = pad_to_block(&signed_bytes, PAD_BLOCK_SIZE);

        // 7. Ephemeral DH key exchange (B2: forward secrecy)
        let (ephemeral_pub_bytes, mut aes_key) = sender_key_exchange(peer_static_public)?;

        // 8. AES-256-GCM encrypt (B6: nonce via OsRng inside encrypt_aes_gcm)
        let (nonce, ciphertext) = encrypt_aes_gcm(&aes_key, &padded)?;

        // 9. Zeroize AES key (A1)
        aes_key.iter_mut().for_each(|b| *b = 0);

        // 10. Compute sender_hint (B5: O(1) sender identification)
        let sender_hint =
            compute_sender_hint(self.identity.x25519_static_secret(), peer_static_public);

        Ok(EncryptedEnvelope {
            version: 1,
            ephemeral_public_key: ephemeral_pub_bytes,
            nonce,
            sender_hint,
            ciphertext,
        })
    }

    /// Open an envelope received from a peer (full pipeline per Part C).
    ///
    /// Pipeline: check nonce → identify sender → DH → HKDF → decrypt → unpad → verify sig → decompress → check timestamp
    pub fn open(
        &self,
        envelope: &EncryptedEnvelope,
        known_peers: &[PeerInfo],
    ) -> Result<(ClearMessage, usize), CryptoError> {
        // 0. Version check
        if envelope.version != 1 {
            return Err(CryptoError::Serialization(format!(
                "unsupported envelope version: {}",
                envelope.version
            )));
        }

        // 1. Anti-replay: check nonce not seen (B4)
        if self.nonce_store.exists(&envelope.nonce)? {
            return Err(CryptoError::ReplayDetected);
        }

        // 2. Identify sender via sender_hint (B5)
        let (peer_index, _peer) = known_peers
            .iter()
            .enumerate()
            .find(|(_, peer)| {
                crate::crypto::key_exchange::verify_sender_hint(
                    self.identity.x25519_static_secret(),
                    &peer.x25519_public,
                    &envelope.sender_hint,
                )
            })
            .ok_or(CryptoError::UnknownSender)?;

        let peer = &known_peers[peer_index];

        // 3. Receiver-side DH (B2 + B3)
        let ephemeral_public = X25519PublicKey::from(envelope.ephemeral_public_key);
        let mut aes_key =
            receiver_key_exchange(self.identity.x25519_static_secret(), &ephemeral_public)?;

        // 4. AES-256-GCM decrypt
        let padded = decrypt_aes_gcm(&aes_key, &envelope.nonce, &envelope.ciphertext)?;

        // Zeroize AES key (A1)
        aes_key.iter_mut().for_each(|b| *b = 0);

        // 5. Remove padding (B9)
        let signed_bytes = unpad(&padded)?;

        // 6. Deserialize SignedPayload (MessagePack)
        let signed_payload: SignedPayload = rmp_serde::from_slice(&signed_bytes)
            .map_err(|e| CryptoError::Serialization(e.to_string()))?;

        // 7. Verify Ed25519 signature (B1: signature was on compressed data)
        let sig_bytes: [u8; 64] = signed_payload
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::InvalidSignature)?;
        let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        peer.verifying_key
            .verify(&signed_payload.data, &signature)
            .map_err(|_| CryptoError::InvalidSignature)?;

        // 8. Decompress
        let json = zstd::decode_all(signed_payload.data.as_slice())
            .map_err(|e| CryptoError::Compression(e.to_string()))?;

        // 9. Deserialize ClearMessage
        let message: ClearMessage =
            serde_json::from_slice(&json).map_err(|e| CryptoError::Serialization(e.to_string()))?;

        // 10. Check timestamp window (B4: ±5 min)
        let now = chrono::Utc::now().timestamp();
        let drift = (now - message.timestamp).abs();
        if drift > MAX_TIMESTAMP_DRIFT_SECS {
            return Err(CryptoError::MessageExpired);
        }

        // 11. Record nonce as seen (B4: anti-replay)
        self.nonce_store.insert(&envelope.nonce)?;

        Ok((message, peer_index))
    }

    /// Access the node identity (for key export, public key sharing, etc.)
    pub fn identity(&self) -> &NodeIdentity {
        &self.identity
    }
}

// ── In-memory NonceStore for testing ──────────────────────────────────

/// Simple in-memory nonce store for unit tests.
///
/// Production uses SQLite `seen_envelopes` table.
#[derive(Default)]
pub struct InMemoryNonceStore {
    seen: std::sync::Mutex<std::collections::HashSet<[u8; 12]>>,
}

impl InMemoryNonceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl NonceStore for InMemoryNonceStore {
    fn exists(&self, nonce: &[u8; 12]) -> Result<bool, CryptoError> {
        Ok(self.seen.lock().unwrap().contains(nonce))
    }

    fn insert(&self, nonce: &[u8; 12]) -> Result<(), CryptoError> {
        self.seen.lock().unwrap().insert(*nonce);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::identity::NodeIdentity;

    fn make_test_message() -> ClearMessage {
        ClearMessage {
            message_type: "loan_request".to_string(),
            payload: serde_json::json!({
                "book_isbn": "978-2-264-02484-8",
                "book_title": "Martin Eden",
            }),
            timestamp: chrono::Utc::now().timestamp(),
            message_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    #[test]
    fn seal_open_roundtrip() {
        let alice = NodeIdentity::generate();
        let bob = NodeIdentity::generate();

        let alice_service = CryptoService::new(alice, InMemoryNonceStore::new());
        let bob_service = CryptoService::new(bob, InMemoryNonceStore::new());

        let message = make_test_message();

        // Alice seals for Bob
        let envelope = alice_service
            .seal(&bob_service.identity().x25519_public_key(), &message)
            .unwrap();

        // Bob opens
        let bob_peers = vec![PeerInfo {
            verifying_key: alice_service.identity().verifying_key(),
            x25519_public: alice_service.identity().x25519_public_key(),
        }];

        let (decrypted, peer_idx) = bob_service.open(&envelope, &bob_peers).unwrap();

        assert_eq!(peer_idx, 0);
        assert_eq!(decrypted.message_type, message.message_type);
        assert_eq!(decrypted.payload, message.payload);
        assert_eq!(decrypted.message_id, message.message_id);
    }

    #[test]
    fn replay_detected() {
        let alice = NodeIdentity::generate();
        let bob = NodeIdentity::generate();

        let alice_service = CryptoService::new(alice, InMemoryNonceStore::new());
        let bob_service = CryptoService::new(bob, InMemoryNonceStore::new());

        let message = make_test_message();
        let envelope = alice_service
            .seal(&bob_service.identity().x25519_public_key(), &message)
            .unwrap();

        let bob_peers = vec![PeerInfo {
            verifying_key: alice_service.identity().verifying_key(),
            x25519_public: alice_service.identity().x25519_public_key(),
        }];

        // First open succeeds
        bob_service.open(&envelope, &bob_peers).unwrap();

        // Second open with same envelope → replay detected
        let result = bob_service.open(&envelope, &bob_peers);
        assert!(matches!(result, Err(CryptoError::ReplayDetected)));
    }

    #[test]
    fn invalid_signature_rejected() {
        let alice = NodeIdentity::generate();
        let bob = NodeIdentity::generate();
        let eve = NodeIdentity::generate(); // attacker

        let alice_service = CryptoService::new(alice, InMemoryNonceStore::new());
        let bob_service = CryptoService::new(bob, InMemoryNonceStore::new());

        let message = make_test_message();
        let envelope = alice_service
            .seal(&bob_service.identity().x25519_public_key(), &message)
            .unwrap();

        // Bob thinks the message is from Eve (wrong verifying key)
        let bob_peers_wrong = vec![PeerInfo {
            verifying_key: eve.verifying_key(),
            x25519_public: alice_service.identity().x25519_public_key(),
        }];

        // This should fail: sender_hint won't match Eve, so UnknownSender
        // (because sender_hint is computed from Alice's static key, not Eve's)
        let result = bob_service.open(&envelope, &bob_peers_wrong);
        // Eve's x25519_public doesn't match Alice's hint → UnknownSender
        assert!(matches!(
            result,
            Err(CryptoError::UnknownSender) | Err(CryptoError::InvalidSignature)
        ));
    }

    #[test]
    fn expired_message_rejected() {
        let alice = NodeIdentity::generate();
        let bob = NodeIdentity::generate();

        let alice_service = CryptoService::new(alice, InMemoryNonceStore::new());
        let bob_service = CryptoService::new(bob, InMemoryNonceStore::new());

        // Create a message with a timestamp 10 minutes in the past
        let mut message = make_test_message();
        message.timestamp = chrono::Utc::now().timestamp() - 600;

        let envelope = alice_service
            .seal(&bob_service.identity().x25519_public_key(), &message)
            .unwrap();

        let bob_peers = vec![PeerInfo {
            verifying_key: alice_service.identity().verifying_key(),
            x25519_public: alice_service.identity().x25519_public_key(),
        }];

        let result = bob_service.open(&envelope, &bob_peers);
        assert!(matches!(result, Err(CryptoError::MessageExpired)));
    }

    #[test]
    fn unknown_sender_rejected() {
        let alice = NodeIdentity::generate();
        let bob = NodeIdentity::generate();
        let charlie = NodeIdentity::generate();

        let alice_service = CryptoService::new(alice, InMemoryNonceStore::new());
        let bob_service = CryptoService::new(bob, InMemoryNonceStore::new());

        let message = make_test_message();
        let envelope = alice_service
            .seal(&bob_service.identity().x25519_public_key(), &message)
            .unwrap();

        // Bob only knows Charlie, not Alice
        let bob_peers = vec![PeerInfo {
            verifying_key: charlie.verifying_key(),
            x25519_public: charlie.x25519_public_key(),
        }];

        let result = bob_service.open(&envelope, &bob_peers);
        assert!(matches!(result, Err(CryptoError::UnknownSender)));
    }

    #[test]
    fn wrong_recipient_cannot_decrypt() {
        let alice = NodeIdentity::generate();
        let bob = NodeIdentity::generate();
        let eve = NodeIdentity::generate();

        let alice_service = CryptoService::new(alice, InMemoryNonceStore::new());
        let eve_service = CryptoService::new(eve, InMemoryNonceStore::new());

        let message = make_test_message();

        // Alice encrypts for Bob
        let bob_public = bob.x25519_public_key();
        let envelope = alice_service.seal(&bob_public, &message).unwrap();

        // Eve tries to open it (even if she knows Alice)
        let eve_peers = vec![PeerInfo {
            verifying_key: alice_service.identity().verifying_key(),
            x25519_public: alice_service.identity().x25519_public_key(),
        }];

        let result = eve_service.open(&envelope, &eve_peers);
        // Eve can't derive the right AES key → decryption fails
        assert!(result.is_err());
    }
}
