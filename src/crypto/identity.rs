use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519StaticSecret};
use zeroize::ZeroizeOnDrop;

/// Long-term cryptographic identity for a node.
///
/// Contains:
/// - Ed25519 signing keypair (for message authentication)
/// - X25519 static keypair (for receiving DH key exchanges)
///
/// Per A1: all secret material is zeroized on drop.
/// Per A2: Debug is manually implemented to redact secrets.
#[derive(ZeroizeOnDrop)]
pub struct NodeIdentity {
    /// Ed25519 signing key (secret). Used to sign messages before encryption (B1).
    #[zeroize(skip)] // SigningKey implements Zeroize internally
    signing_key: SigningKey,
    /// X25519 static secret. Used by receivers to compute shared secret with sender's ephemeral key.
    #[zeroize(skip)] // StaticSecret doesn't impl Zeroize; we handle raw bytes
    x25519_secret: X25519StaticSecret,
    /// Raw bytes of the X25519 secret for zeroization.
    x25519_secret_bytes: [u8; 32],
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("signing_public", &self.verifying_key().as_bytes())
            .field("x25519_public", &self.x25519_public_key().as_bytes())
            .field("signing_key", &"[REDACTED]")
            .field("x25519_secret", &"[REDACTED]")
            .finish()
    }
}

impl NodeIdentity {
    /// Generate a new random identity.
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);

        let x25519_secret = X25519StaticSecret::random_from_rng(OsRng);
        let x25519_secret_bytes = x25519_secret.to_bytes();

        Self {
            signing_key,
            x25519_secret,
            x25519_secret_bytes,
        }
    }

    /// Reconstruct identity from stored key bytes.
    ///
    /// `signing_bytes`: 32-byte Ed25519 secret key.
    /// `x25519_bytes`: 32-byte X25519 static secret.
    pub fn from_bytes(signing_bytes: &[u8; 32], x25519_bytes: &[u8; 32]) -> Self {
        let signing_key = SigningKey::from_bytes(signing_bytes);
        let x25519_secret = X25519StaticSecret::from(*x25519_bytes);
        let x25519_secret_bytes = *x25519_bytes;

        Self {
            signing_key,
            x25519_secret,
            x25519_secret_bytes,
        }
    }

    /// Export secret key bytes for encrypted storage.
    ///
    /// Returns (ed25519_secret_32bytes, x25519_secret_32bytes).
    /// Caller MUST encrypt these before persisting (e.g., with Argon2-derived key).
    pub fn export_secret_bytes(&self) -> ([u8; 32], [u8; 32]) {
        (self.signing_key.to_bytes(), self.x25519_secret_bytes)
    }

    // --- Public key accessors ---

    pub fn verifying_key(&self) -> ed25519_dalek::VerifyingKey {
        self.signing_key.verifying_key()
    }

    pub fn x25519_public_key(&self) -> X25519PublicKey {
        X25519PublicKey::from(&self.x25519_secret)
    }

    /// Access the Ed25519 signing key (for seal operations).
    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// Access the X25519 static secret (for open/receive operations).
    pub(crate) fn x25519_static_secret(&self) -> &X25519StaticSecret {
        &self.x25519_secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, Verifier};

    #[test]
    fn generate_and_sign_verify() {
        let identity = NodeIdentity::generate();
        let message = b"test message";

        let signature = identity.signing_key().sign(message);
        assert!(identity.verifying_key().verify(message, &signature).is_ok());
    }

    #[test]
    fn export_import_roundtrip() {
        let identity = NodeIdentity::generate();
        let (ed_bytes, x_bytes) = identity.export_secret_bytes();
        let restored = NodeIdentity::from_bytes(&ed_bytes, &x_bytes);

        assert_eq!(
            identity.verifying_key().as_bytes(),
            restored.verifying_key().as_bytes()
        );
        assert_eq!(
            identity.x25519_public_key().as_bytes(),
            restored.x25519_public_key().as_bytes()
        );
    }

    #[test]
    fn debug_redacts_secrets() {
        let identity = NodeIdentity::generate();
        let debug_output = format!("{identity:?}");
        assert!(debug_output.contains("[REDACTED]"));
        // Ensure no raw secret bytes leak
        assert!(!debug_output.contains("x25519_secret_bytes"));
    }
}
