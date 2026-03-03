use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey, StaticSecret};

use super::encryption::derive_aes_key;
use super::errors::CryptoError;

type HmacSha256 = Hmac<Sha256>;

/// Sender-side: perform ephemeral DH and derive the AES key (B2 + B3).
///
/// Returns (ephemeral_public_key, derived_aes_key).
/// The `EphemeralSecret` is consumed (move semantics) — cannot be reused.
pub fn sender_key_exchange(
    peer_static_public: &X25519PublicKey,
) -> Result<([u8; 32], [u8; 32]), CryptoError> {
    let ephemeral_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
    let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);

    // DH: ephemeral_secret × peer_static_public
    let shared_secret = ephemeral_secret.diffie_hellman(peer_static_public);

    // HKDF derivation (B3: never use raw DH output)
    let aes_key = derive_aes_key(shared_secret.as_bytes())?;

    Ok((*ephemeral_public.as_bytes(), aes_key))
}

/// Receiver-side: derive the AES key from sender's ephemeral public key (B2 + B3).
pub fn receiver_key_exchange(
    my_static_secret: &StaticSecret,
    sender_ephemeral_public: &X25519PublicKey,
) -> Result<[u8; 32], CryptoError> {
    // DH: my_static_secret × sender_ephemeral_public
    let shared_secret = my_static_secret.diffie_hellman(sender_ephemeral_public);

    // HKDF derivation (B3)
    derive_aes_key(shared_secret.as_bytes())
}

/// Compute sender_hint for O(1) sender identification (B5).
///
/// Uses HMAC-SHA256 over the static shared secret between two peers.
/// The hint is 8 bytes — enough for a fast lookup, opaque to the Hub.
pub fn compute_sender_hint(
    my_static_secret: &StaticSecret,
    peer_static_public: &X25519PublicKey,
) -> [u8; 8] {
    // Static DH between the two long-term keys
    let static_shared = my_static_secret.diffie_hellman(peer_static_public);

    let mut mac =
        HmacSha256::new_from_slice(static_shared.as_bytes()).expect("HMAC accepts any key size");
    mac.update(b"sender-hint");
    let result = mac.finalize().into_bytes();

    let mut hint = [0u8; 8];
    hint.copy_from_slice(&result[..8]);
    hint
}

/// Verify that a received sender_hint matches a candidate peer (B5).
///
/// The receiver computes the hint for each known peer and compares.
pub fn verify_sender_hint(
    my_static_secret: &StaticSecret,
    candidate_peer_public: &X25519PublicKey,
    received_hint: &[u8; 8],
) -> bool {
    let computed = compute_sender_hint(my_static_secret, candidate_peer_public);
    // Constant-time comparison to prevent timing attacks
    constant_time_eq(&computed, received_hint)
}

/// Compute a full 32-byte HMAC for disconnect authentication.
///
/// Uses the static DH shared secret between two peers with the label "disconnect:"
/// (domain-separated from "sender-hint"). Includes library_uuid and timestamp
/// to bind the HMAC to a specific disconnect event.
///
/// The DH is commutative: Alice(secret) x Bob(public) = Bob(secret) x Alice(public),
/// so either side can compute and verify.
pub fn compute_disconnect_hmac(
    my_static_secret: &StaticSecret,
    peer_static_public: &X25519PublicKey,
    library_uuid: &str,
    timestamp: &str,
) -> [u8; 32] {
    let static_shared = my_static_secret.diffie_hellman(peer_static_public);

    let mut mac =
        HmacSha256::new_from_slice(static_shared.as_bytes()).expect("HMAC accepts any key size");
    mac.update(b"disconnect:");
    mac.update(library_uuid.as_bytes());
    mac.update(b":");
    mac.update(timestamp.as_bytes());
    let result = mac.finalize().into_bytes();

    let mut hmac_bytes = [0u8; 32];
    hmac_bytes.copy_from_slice(&result);
    hmac_bytes
}

/// Verify a received disconnect HMAC against the expected value.
///
/// Recomputes the HMAC using the local static secret and the peer's public key,
/// then compares in constant time.
pub fn verify_disconnect_hmac(
    my_static_secret: &StaticSecret,
    peer_static_public: &X25519PublicKey,
    library_uuid: &str,
    timestamp: &str,
    received: &[u8; 32],
) -> bool {
    let computed = compute_disconnect_hmac(
        my_static_secret,
        peer_static_public,
        library_uuid,
        timestamp,
    );
    constant_time_eq(&computed, received)
}

/// Constant-time byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::StaticSecret;

    #[test]
    fn sender_receiver_derive_same_key() {
        // Receiver's static keypair
        let receiver_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let receiver_public = X25519PublicKey::from(&receiver_secret);

        // Sender performs ephemeral DH
        let (ephemeral_pub_bytes, sender_aes_key) = sender_key_exchange(&receiver_public).unwrap();

        // Receiver derives the same key
        let ephemeral_public = X25519PublicKey::from(ephemeral_pub_bytes);
        let receiver_aes_key = receiver_key_exchange(&receiver_secret, &ephemeral_public).unwrap();

        assert_eq!(sender_aes_key, receiver_aes_key);
    }

    #[test]
    fn different_receivers_get_different_keys() {
        let receiver1_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let receiver1_public = X25519PublicKey::from(&receiver1_secret);

        let receiver2_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let receiver2_public = X25519PublicKey::from(&receiver2_secret);

        let (_, key1) = sender_key_exchange(&receiver1_public).unwrap();
        let (_, key2) = sender_key_exchange(&receiver2_public).unwrap();

        // Each call generates a new ephemeral secret, so keys differ
        assert_ne!(key1, key2);
    }

    #[test]
    fn sender_hint_matches_for_correct_peer() {
        let alice_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let alice_public = X25519PublicKey::from(&alice_secret);

        let bob_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_public = X25519PublicKey::from(&bob_secret);

        // Alice computes hint for Bob
        let hint = compute_sender_hint(&alice_secret, &bob_public);

        // Bob verifies: is this from Alice?
        assert!(verify_sender_hint(&bob_secret, &alice_public, &hint));
    }

    #[test]
    fn sender_hint_rejects_wrong_peer() {
        let alice_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let alice_public = X25519PublicKey::from(&alice_secret);

        let bob_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_public = X25519PublicKey::from(&bob_secret);

        let eve_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let eve_public = X25519PublicKey::from(&eve_secret);

        // Alice computes hint for Bob
        let hint = compute_sender_hint(&alice_secret, &bob_public);

        // Bob checks against Eve — should NOT match
        assert!(!verify_sender_hint(&bob_secret, &eve_public, &hint));
        // Eve checks — should NOT match
        assert!(!verify_sender_hint(&eve_secret, &alice_public, &hint));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"abcdefgh", b"abcdefgh"));
        assert!(!constant_time_eq(b"abcdefgh", b"abcdefgX"));
        assert!(!constant_time_eq(b"short", b"longer__"));
    }

    #[test]
    fn disconnect_hmac_roundtrip() {
        let alice_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let alice_public = X25519PublicKey::from(&alice_secret);

        let bob_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_public = X25519PublicKey::from(&bob_secret);

        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let timestamp = "2026-03-03T12:00:00Z";

        // Alice computes HMAC for Bob
        let hmac = compute_disconnect_hmac(&alice_secret, &bob_public, uuid, timestamp);

        // Bob verifies (commutative DH)
        assert!(verify_disconnect_hmac(
            &bob_secret,
            &alice_public,
            uuid,
            timestamp,
            &hmac
        ));
    }

    #[test]
    fn disconnect_hmac_rejects_wrong_uuid() {
        let alice_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_public = X25519PublicKey::from(&bob_secret);
        let alice_public = X25519PublicKey::from(&alice_secret);

        let timestamp = "2026-03-03T12:00:00Z";

        let hmac = compute_disconnect_hmac(&alice_secret, &bob_public, "original-uuid", timestamp);

        // Tampered uuid must fail
        assert!(!verify_disconnect_hmac(
            &bob_secret,
            &alice_public,
            "tampered-uuid",
            timestamp,
            &hmac
        ));
    }

    #[test]
    fn disconnect_hmac_rejects_wrong_peer() {
        let alice_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let bob_public = X25519PublicKey::from(&bob_secret);

        let eve_secret = StaticSecret::random_from_rng(rand::rngs::OsRng);
        let eve_public = X25519PublicKey::from(&eve_secret);

        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let timestamp = "2026-03-03T12:00:00Z";

        // Alice computes HMAC for Bob
        let hmac = compute_disconnect_hmac(&alice_secret, &bob_public, uuid, timestamp);

        // Eve cannot verify (different shared secret)
        assert!(!verify_disconnect_hmac(
            &eve_secret,
            &eve_public,
            uuid,
            timestamp,
            &hmac
        ));
    }
}
