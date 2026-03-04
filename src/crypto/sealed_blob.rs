//! Sealed blob: one-shot asymmetric encryption for hub-mediated contact sharing.
//!
//! Uses ephemeral DH (X25519) + AES-256-GCM to encrypt data for a specific
//! recipient. The hub only sees opaque blobs - it never has access to the
//! plaintext or the decryption key.
//!
//! Wire format: ephemeral_public_key (32) || nonce (12) || ciphertext (variable)

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use super::encryption::{decrypt_aes_gcm, encrypt_aes_gcm};
use super::errors::CryptoError;
use super::key_exchange::{receiver_key_exchange, sender_key_exchange};

const HEADER_LEN: usize = 32 + 12; // ephemeral_public_key + nonce

/// Encrypt `plaintext` so that only the holder of `recipient_x25519_secret` can decrypt it.
///
/// Returns a base64-encoded sealed blob ready for hub storage.
pub fn seal(recipient_x25519_public: &[u8; 32], plaintext: &[u8]) -> Result<String, CryptoError> {
    let public_key = X25519PublicKey::from(*recipient_x25519_public);
    let (ephemeral_pub, aes_key) = sender_key_exchange(&public_key)?;
    let (nonce, ciphertext) = encrypt_aes_gcm(&aes_key, plaintext)?;

    // Wire format: ephemeral_pub (32) || nonce (12) || ciphertext
    let mut blob = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    blob.extend_from_slice(&ephemeral_pub);
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(&blob))
}

/// Decrypt a base64-encoded sealed blob using the recipient's static secret.
pub fn open(my_x25519_secret: &StaticSecret, sealed_base64: &str) -> Result<Vec<u8>, CryptoError> {
    let blob = BASE64
        .decode(sealed_base64)
        .map_err(|e| CryptoError::Serialization(format!("base64 decode: {e}")))?;

    if blob.len() < HEADER_LEN + 1 {
        return Err(CryptoError::Serialization(
            "sealed blob too short".to_string(),
        ));
    }

    let ephemeral_pub: [u8; 32] = blob[..32]
        .try_into()
        .map_err(|_| CryptoError::Serialization("bad ephemeral key".to_string()))?;
    let nonce: [u8; 12] = blob[32..44]
        .try_into()
        .map_err(|_| CryptoError::Serialization("bad nonce".to_string()))?;
    let ciphertext = &blob[44..];

    let ephemeral_public = X25519PublicKey::from(ephemeral_pub);
    let aes_key = receiver_key_exchange(my_x25519_secret, &ephemeral_public)?;

    decrypt_aes_gcm(&aes_key, &nonce, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::StaticSecret;

    #[test]
    fn seal_open_roundtrip() {
        let recipient_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let recipient_public = X25519PublicKey::from(&recipient_secret);

        let plaintext = b"Contact: alice@example.com - Available Tue/Thu 2-6pm";
        let sealed = seal(recipient_public.as_bytes(), plaintext).unwrap();

        let decrypted = open(&recipient_secret, &sealed).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_cannot_open() {
        let recipient_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let recipient_public = X25519PublicKey::from(&recipient_secret);

        let eve_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);

        let sealed = seal(recipient_public.as_bytes(), b"secret contact").unwrap();

        let result = open(&eve_secret, &sealed);
        assert!(result.is_err());
    }

    #[test]
    fn tampered_blob_fails() {
        let recipient_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let recipient_public = X25519PublicKey::from(&recipient_secret);

        let sealed = seal(recipient_public.as_bytes(), b"secret").unwrap();
        let mut bytes = BASE64.decode(&sealed).unwrap();
        // Flip a byte in the ciphertext
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let tampered = BASE64.encode(&bytes);

        let result = open(&recipient_secret, &tampered);
        assert!(result.is_err());
    }

    #[test]
    fn too_short_blob_fails() {
        let secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let short = BASE64.encode(&[0u8; 10]);
        let result = open(&secret, &short);
        assert!(result.is_err());
    }

    #[test]
    fn empty_plaintext_works() {
        let recipient_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let recipient_public = X25519PublicKey::from(&recipient_secret);

        let sealed = seal(recipient_public.as_bytes(), b"").unwrap();
        let decrypted = open(&recipient_secret, &sealed).unwrap();
        assert!(decrypted.is_empty());
    }
}
