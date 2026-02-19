use serde::{Deserialize, Serialize};

/// Encrypted envelope sent over the wire.
///
/// Per B1: NO signature field here — it's hidden inside the ciphertext.
/// Per B2: ephemeral_public_key enables forward secrecy.
/// Per B5: sender_hint enables O(1) sender identification without exposing identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    /// Protocol version (currently 1). Allows future migration (Joplin lesson).
    pub version: u8,
    /// Ephemeral X25519 public key for this message (B2: forward secrecy).
    pub ephemeral_public_key: [u8; 32],
    /// AES-GCM nonce (12 bytes, random via OsRng) (B6).
    pub nonce: [u8; 12],
    /// HMAC-SHA256(static_shared_secret, "sender-hint")[..8] (B5).
    pub sender_hint: [u8; 8],
    /// Encrypted SignedPayload (contains signature + compressed message).
    pub ciphertext: Vec<u8>,
}

/// Signed payload — lives INSIDE the ciphertext (B1: sign-then-encrypt).
///
/// Serialized with MessagePack (rmp-serde) for compactness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedPayload {
    /// zstd-compressed, then padded ClearMessage JSON.
    pub data: Vec<u8>,
    /// Ed25519 signature over `data` (64 bytes, stored as Vec for serde compat).
    pub signature: Vec<u8>,
}

/// Cleartext message — the application-level payload.
///
/// Serialized to JSON, then compressed with zstd before signing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearMessage {
    /// Message type discriminator (e.g., "loan_request", "loan_confirmation", "key_exchange").
    pub message_type: String,
    /// JSON-encoded payload specific to message_type.
    pub payload: serde_json::Value,
    /// Unix timestamp (seconds) — used for expiry check (B4: ±5 min window).
    pub timestamp: i64,
    /// Random message nonce (for deduplication, separate from AES nonce).
    pub message_id: String,
}
