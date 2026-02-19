use std::fmt;

/// Crypto-specific errors — framework-agnostic (domain-level).
#[derive(Debug)]
pub enum CryptoError {
    /// AES-GCM decryption failed (wrong key, tampered ciphertext, or bad nonce).
    DecryptionFailed,
    /// Ed25519 signature verification failed.
    InvalidSignature,
    /// Nonce already seen — possible replay attack (B4).
    ReplayDetected,
    /// Message timestamp outside acceptable window (±5 min) (B4).
    MessageExpired,
    /// HKDF key derivation failed.
    KeyDerivationFailed,
    /// Serialization or deserialization error.
    Serialization(String),
    /// Compression or decompression error.
    Compression(String),
    /// No matching peer found for sender_hint (B5).
    UnknownSender,
    /// Identity not initialized or key material unavailable.
    IdentityNotAvailable,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecryptionFailed => write!(f, "decryption failed"),
            Self::InvalidSignature => write!(f, "invalid signature"),
            Self::ReplayDetected => write!(f, "replay detected"),
            Self::MessageExpired => write!(f, "message expired"),
            Self::KeyDerivationFailed => write!(f, "key derivation failed"),
            Self::Serialization(msg) => write!(f, "serialization error: {msg}"),
            Self::Compression(msg) => write!(f, "compression error: {msg}"),
            Self::UnknownSender => write!(f, "unknown sender"),
            Self::IdentityNotAvailable => write!(f, "identity not available"),
        }
    }
}

impl std::error::Error for CryptoError {}
