use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use zeroize::Zeroize;

use super::errors::CryptoError;

/// HKDF info string — unique to our protocol, prevents cross-protocol attacks.
const HKDF_INFO: &[u8] = b"bibliogenius-e2ee-v1-message-key";

/// Derive a 256-bit AES key from a DH shared secret using HKDF-SHA256 (B3).
///
/// NEVER use the raw DH output directly as an encryption key.
pub fn derive_aes_key(shared_secret: &[u8]) -> Result<[u8; 32], CryptoError> {
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret);
    let mut aes_key = [0u8; 32];
    hkdf.expand(HKDF_INFO, &mut aes_key)
        .map_err(|_| CryptoError::KeyDerivationFailed)?;
    Ok(aes_key)
}

/// Generate a random 12-byte nonce via OsRng (B6).
///
/// With ephemeral keys per message, nonce collision risk is negligible,
/// but we still use random nonces as defense in depth.
pub fn generate_nonce() -> [u8; 12] {
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Encrypt plaintext with AES-256-GCM.
///
/// Returns (nonce, ciphertext) — nonce is included for the receiver.
pub fn encrypt_aes_gcm(
    key: &[u8; 32],
    plaintext: &[u8],
) -> Result<([u8; 12], Vec<u8>), CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::KeyDerivationFailed)?;
    let nonce_bytes = generate_nonce();
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    Ok((nonce_bytes, ciphertext))
}

/// Decrypt ciphertext with AES-256-GCM.
pub fn decrypt_aes_gcm(
    key: &[u8; 32],
    nonce: &[u8; 12],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::KeyDerivationFailed)?;
    let nonce = Nonce::from_slice(nonce);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| CryptoError::DecryptionFailed)
}

/// Pad data to a multiple of `block_size` bytes (B9: anti-CRIME).
///
/// Prepends a 4-byte little-endian length header so the receiver can unpad.
pub fn pad_to_block(data: &[u8], block_size: usize) -> Vec<u8> {
    let len = data.len() as u32;
    let total = 4 + data.len(); // 4-byte length prefix + data
    let padded_len = ((total / block_size) + 1) * block_size;
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(&len.to_le_bytes());
    padded.extend_from_slice(data);
    padded.resize(padded_len, 0);
    padded
}

/// Remove padding added by `pad_to_block`.
pub fn unpad(padded: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if padded.len() < 4 {
        return Err(CryptoError::Serialization(
            "padded data too short".to_string(),
        ));
    }
    let len = u32::from_le_bytes(
        padded[..4]
            .try_into()
            .map_err(|_| CryptoError::Serialization("invalid length prefix".to_string()))?,
    ) as usize;
    if 4 + len > padded.len() {
        return Err(CryptoError::Serialization(
            "length prefix exceeds data".to_string(),
        ));
    }
    Ok(padded[4..4 + len].to_vec())
}

/// Derive Argon2id key from a password (B7).
///
/// Used for encrypting the NodeIdentity at rest.
pub fn derive_key_from_password(password: &[u8], salt: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    use argon2::{Algorithm, Argon2, Params, Version};

    // OWASP 2024 minimum parameters
    const ARGON2_M_COST: u32 = 65536; // 64 MiB memory
    const ARGON2_T_COST: u32 = 3; // 3 iterations
    const ARGON2_P_COST: u32 = 4; // 4 parallel threads

    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, Some(32))
        .map_err(|_| CryptoError::KeyDerivationFailed)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password, salt, &mut key)
        .map_err(|_| CryptoError::KeyDerivationFailed)?;
    Ok(key)
}

/// Generate a random 32-byte salt for Argon2.
pub fn generate_salt() -> [u8; 32] {
    let mut salt = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    salt
}

/// Zeroize an AES key after use.
pub fn zeroize_key(key: &mut [u8; 32]) {
    key.zeroize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"hello, encrypted world!";

        let (nonce, ciphertext) = encrypt_aes_gcm(&key, plaintext).unwrap();
        let decrypted = decrypt_aes_gcm(&key, &nonce, &ciphertext).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key = [42u8; 32];
        let wrong_key = [99u8; 32];
        let plaintext = b"secret";

        let (nonce, ciphertext) = encrypt_aes_gcm(&key, plaintext).unwrap();
        let result = decrypt_aes_gcm(&wrong_key, &nonce, &ciphertext);

        assert!(result.is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = [42u8; 32];
        let plaintext = b"secret";

        let (nonce, mut ciphertext) = encrypt_aes_gcm(&key, plaintext).unwrap();
        ciphertext[0] ^= 0xFF; // flip bits
        let result = decrypt_aes_gcm(&key, &nonce, &ciphertext);

        assert!(result.is_err());
    }

    #[test]
    fn pad_unpad_roundtrip() {
        let data = b"some data to pad";
        let padded = pad_to_block(data, 256);

        assert_eq!(padded.len() % 256, 0);
        assert!(padded.len() >= data.len() + 4);

        let unpadded = unpad(&padded).unwrap();
        assert_eq!(unpadded, data);
    }

    #[test]
    fn hkdf_produces_deterministic_key() {
        let shared_secret = [7u8; 32];
        let key1 = derive_aes_key(&shared_secret).unwrap();
        let key2 = derive_aes_key(&shared_secret).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn different_secrets_produce_different_keys() {
        let secret1 = [7u8; 32];
        let secret2 = [8u8; 32];
        let key1 = derive_aes_key(&secret1).unwrap();
        let key2 = derive_aes_key(&secret2).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn argon2_derive_key_roundtrip() {
        let password = b"my-strong-password";
        let salt = generate_salt();
        let key1 = derive_key_from_password(password, &salt).unwrap();
        let key2 = derive_key_from_password(password, &salt).unwrap();
        assert_eq!(key1, key2);

        // Different password → different key
        let key3 = derive_key_from_password(b"other-password", &salt).unwrap();
        assert_ne!(key1, key3);
    }
}
