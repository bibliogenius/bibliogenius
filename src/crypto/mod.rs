//! E2EE cryptographic primitives for BiblioGenius.
//!
//! Implements the corrected pipeline from SECURITY_GUIDELINES.md:
//! - Sign-then-encrypt (B1)
//! - Ephemeral DH per message (B2)
//! - HKDF key derivation (B3)
//! - Replay protection via nonce store (B4)
//! - O(1) sender identification via sender_hint (B5)

pub mod encryption;
pub mod envelope;
pub mod errors;
pub mod identity;
pub mod key_exchange;
