//! End-to-end encrypted directory catalog (ADR-072).
//!
//! One random catalog key per library encrypts the catalog the hub caches
//! for peers; the key itself travels to each reader wrapped with
//! [`super::sealed_blob`] (ephemeral X25519, HKDF, AES-GCM), so the hub
//! stores a ciphertext it has no key for.
//!
//! Pipeline, owner side:
//! ```text
//! entries JSON --zstd--> compressed
//! version(u64 LE) || compressed --pad_bucket--> padded
//! AES-256-GCM-SIV(key, nonce = OsRng96,
//!                 aad = "bg-catalog-v1|" || node_id || "|" || key_id_hex,
//!                 plaintext = padded)
//! blob = "BGC1" || key_id(16) || nonce(12) || ciphertext   (base64 standard)
//! ```
//! GCM-SIV because the key is long-lived and encrypts every push (B6).
//! The associated data binds the ciphertext to the library and the key, so
//! the hub cannot serve one library's catalog under another node id, and a
//! reader holding an outdated wrap fails on the key id before it decrypts
//! anything. The version lives inside the plaintext: the reader keeps the
//! highest one seen and refuses a lower one (rollback, B4 adapted). No
//! signature by decision (ADR-072 D2): the catalog has one writer, gated by
//! the hub write token, and the AEAD already rejects any other ciphertext.
//!
//! Rule A1: the key is zeroized on drop and never printed.

use std::io::Read;

use aes_gcm_siv::{
    Aes256GcmSiv, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use x25519_dalek::StaticSecret;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::account_keys::{pad_bucket, unpad_bucket};
use super::errors::CryptoError;
use super::sealed_blob;

/// Value of the hub's `catalog_format` field for this pipeline.
pub const CATALOG_FORMAT_E2EE_V1: &str = "e2ee-v1";

const MAGIC: &[u8; 4] = b"BGC1";
const AAD_PREFIX: &[u8] = b"bg-catalog-v1|";
const KEY_ID_LEN: usize = 16;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + KEY_ID_LEN + NONCE_LEN;
const VERSION_LEN: usize = 8;
/// First byte of a wrapped key, so a wrap can evolve without guessing.
const WRAP_FORMAT_V1: u8 = 1;
const WRAP_PLAINTEXT_LEN: usize = 1 + KEY_ID_LEN + KEY_LEN;
/// A catalog weighs about 280 bytes per book before compression; this bound
/// stops a hostile ciphertext from inflating into the reader's memory.
const MAX_PLAINTEXT_LEN: usize = 8 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 3;

/// The catalog key of one library: a random 256-bit key and the 128-bit id
/// that names it in every ciphertext and every wrap.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct CatalogKey {
    key_id: [u8; KEY_ID_LEN],
    key: [u8; KEY_LEN],
}

impl std::fmt::Debug for CatalogKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogKey")
            .field("key_id", &self.key_id_hex())
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl CatalogKey {
    /// A fresh key with a fresh id, both from the OS random source.
    pub fn generate() -> Self {
        let mut key_id = [0u8; KEY_ID_LEN];
        let mut key = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut key_id);
        OsRng.fill_bytes(&mut key);
        Self { key_id, key }
    }

    /// Rebuilds a key from stored parts (the at-rest store keeps the id in
    /// clear next to the encrypted key bytes).
    pub fn from_parts(key_id: [u8; KEY_ID_LEN], key: [u8; KEY_LEN]) -> Self {
        Self { key_id, key }
    }

    pub fn key_id(&self) -> &[u8; KEY_ID_LEN] {
        &self.key_id
    }

    pub fn key_id_hex(&self) -> String {
        hex::encode(self.key_id)
    }

    /// The raw key bytes, for the at-rest store only. Zeroized on drop.
    pub fn export_secret(&self) -> Zeroizing<[u8; KEY_LEN]> {
        Zeroizing::new(self.key)
    }

    fn aad(&self, node_id: &str) -> Vec<u8> {
        let mut aad = Vec::with_capacity(AAD_PREFIX.len() + node_id.len() + 1 + KEY_ID_LEN * 2);
        aad.extend_from_slice(AAD_PREFIX);
        aad.extend_from_slice(node_id.as_bytes());
        aad.push(b'|');
        aad.extend_from_slice(self.key_id_hex().as_bytes());
        aad
    }

    /// Encrypts one catalog version for the hub. `plaintext` is the clear
    /// catalog payload (the JSON list of entries); `node_id` is the owner's
    /// hub identity the ciphertext is bound to.
    pub fn encrypt(
        &self,
        node_id: &str,
        version: u64,
        plaintext: &[u8],
    ) -> Result<String, CryptoError> {
        if plaintext.len() > MAX_PLAINTEXT_LEN {
            return Err(CryptoError::Serialization(
                "catalog too large to encrypt".to_string(),
            ));
        }
        let compressed = zstd::stream::encode_all(plaintext, ZSTD_LEVEL)
            .map_err(|e| CryptoError::Compression(e.to_string()))?;

        let mut body = Zeroizing::new(Vec::with_capacity(VERSION_LEN + compressed.len()));
        body.extend_from_slice(&version.to_le_bytes());
        body.extend_from_slice(&compressed);
        let padded = Zeroizing::new(pad_bucket(&body));

        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let cipher = Aes256GcmSiv::new_from_slice(&self.key)
            .map_err(|_| CryptoError::KeyDerivationFailed)?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &padded,
                    aad: &self.aad(node_id),
                },
            )
            .map_err(|_| CryptoError::DecryptionFailed)?;

        let mut blob = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        blob.extend_from_slice(MAGIC);
        blob.extend_from_slice(&self.key_id);
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(&blob))
    }

    /// Opens a catalog ciphertext produced by [`Self::encrypt`] under this
    /// key for `node_id`. Returns the version carried inside and the clear
    /// payload. A blob naming another key id fails with
    /// [`CryptoError::KeyIdMismatch`] before any decryption is attempted.
    pub fn decrypt(&self, node_id: &str, blob_base64: &str) -> Result<(u64, Vec<u8>), CryptoError> {
        let blob = decode_blob(blob_base64)?;
        if blob[MAGIC.len()..MAGIC.len() + KEY_ID_LEN] != self.key_id {
            return Err(CryptoError::KeyIdMismatch);
        }
        let nonce_start = MAGIC.len() + KEY_ID_LEN;
        let nonce = Nonce::from_slice(&blob[nonce_start..HEADER_LEN]);
        let cipher = Aes256GcmSiv::new_from_slice(&self.key)
            .map_err(|_| CryptoError::KeyDerivationFailed)?;
        let padded = Zeroizing::new(
            cipher
                .decrypt(
                    nonce,
                    Payload {
                        msg: &blob[HEADER_LEN..],
                        aad: &self.aad(node_id),
                    },
                )
                .map_err(|_| CryptoError::DecryptionFailed)?,
        );
        let body = Zeroizing::new(unpad_bucket(&padded)?);
        if body.len() < VERSION_LEN {
            return Err(CryptoError::Serialization(
                "catalog body too short".to_string(),
            ));
        }
        let version = u64::from_le_bytes(
            body[..VERSION_LEN]
                .try_into()
                .map_err(|_| CryptoError::Serialization("invalid version".to_string()))?,
        );
        let plaintext = decompress_bounded(&body[VERSION_LEN..])?;
        Ok((version, plaintext))
    }

    /// Keyed digest standing in for the clear `catalog_hash` (ADR-027 skip):
    /// same length-prefixed input as the clear hash, so it is stable while
    /// the catalog is, but useless to anyone without the key for confirming
    /// a guessed catalog (ADR-072 D6).
    pub fn catalog_hash(
        &self,
        isbn_payload: &str,
        catalog_payload: &str,
        book_count: i64,
    ) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key)
            .expect("HMAC-SHA256 accepts any key length");
        for field in [isbn_payload.as_bytes(), catalog_payload.as_bytes()] {
            mac.update(&(field.len() as u64).to_le_bytes());
            mac.update(field);
        }
        mac.update(&book_count.to_le_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Seals this key for one reader, identified by its X25519 public key.
    /// Each wrap is an independent ephemeral-DH sealed blob (B2 on the wrap).
    pub fn wrap_for(&self, reader_x25519_public: &[u8; 32]) -> Result<String, CryptoError> {
        let mut plaintext = Zeroizing::new(Vec::with_capacity(WRAP_PLAINTEXT_LEN));
        plaintext.push(WRAP_FORMAT_V1);
        plaintext.extend_from_slice(&self.key_id);
        plaintext.extend_from_slice(&self.key);
        sealed_blob::seal(reader_x25519_public, &plaintext)
    }

    /// Opens a wrap produced by [`Self::wrap_for`] with the reader's static
    /// X25519 secret.
    pub fn unwrap(
        reader_x25519_secret: &StaticSecret,
        wrap_base64: &str,
    ) -> Result<Self, CryptoError> {
        let plaintext = Zeroizing::new(sealed_blob::open(reader_x25519_secret, wrap_base64)?);
        if plaintext.len() != WRAP_PLAINTEXT_LEN || plaintext[0] != WRAP_FORMAT_V1 {
            return Err(CryptoError::Serialization(
                "unsupported catalog key wrap".to_string(),
            ));
        }
        let mut key_id = [0u8; KEY_ID_LEN];
        let mut key = [0u8; KEY_LEN];
        key_id.copy_from_slice(&plaintext[1..1 + KEY_ID_LEN]);
        key.copy_from_slice(&plaintext[1 + KEY_ID_LEN..]);
        Ok(Self { key_id, key })
    }
}

/// The key id a catalog ciphertext was produced under, read from its clear
/// header. Lets a reader tell "my wrap is outdated" from "the ciphertext is
/// broken" without holding the right key.
pub fn key_id_of_blob(blob_base64: &str) -> Result<String, CryptoError> {
    let blob = decode_blob(blob_base64)?;
    Ok(hex::encode(&blob[MAGIC.len()..MAGIC.len() + KEY_ID_LEN]))
}

fn decode_blob(blob_base64: &str) -> Result<Vec<u8>, CryptoError> {
    let blob = BASE64
        .decode(blob_base64)
        .map_err(|e| CryptoError::Serialization(format!("base64 decode: {e}")))?;
    if blob.len() < HEADER_LEN + TAG_LEN || &blob[..MAGIC.len()] != MAGIC {
        return Err(CryptoError::Serialization(
            "not a catalog ciphertext".to_string(),
        ));
    }
    Ok(blob)
}

/// zstd decode capped at [`MAX_PLAINTEXT_LEN`]: a ciphertext is accepted
/// from the hub, which must not be able to make a reader allocate without
/// bound.
fn decompress_bounded(compressed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|e| CryptoError::Compression(e.to_string()))?;
    let mut plaintext = Vec::new();
    decoder
        .take(MAX_PLAINTEXT_LEN as u64 + 1)
        .read_to_end(&mut plaintext)
        .map_err(|e| CryptoError::Compression(e.to_string()))?;
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(CryptoError::Compression(
            "catalog exceeds the decompression bound".to_string(),
        ));
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::PublicKey as X25519PublicKey;

    const NODE: &str = "owner-node-id";
    const CATALOG: &[u8] = br#"[{"isbn":"9782070360024","book_uuid":"0197f2a4-1111-7222-8333-444455556666","title":"L'Etranger","author":"Albert Camus"}]"#;

    fn reader() -> (StaticSecret, [u8; 32]) {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = *X25519PublicKey::from(&secret).as_bytes();
        (secret, public)
    }

    #[test]
    fn encrypt_decrypt_roundtrip_keeps_version_and_payload() {
        let key = CatalogKey::generate();
        let blob = key.encrypt(NODE, 7, CATALOG).unwrap();

        let (version, plaintext) = key.decrypt(NODE, &blob).unwrap();

        assert_eq!(version, 7);
        assert_eq!(plaintext, CATALOG);
        assert_eq!(key_id_of_blob(&blob).unwrap(), key.key_id_hex());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let key = CatalogKey::generate();
        let blob = key.encrypt(NODE, 1, CATALOG).unwrap();
        let mut bytes = BASE64.decode(&blob).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;

        assert!(matches!(
            key.decrypt(NODE, &BASE64.encode(&bytes)),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn ciphertext_moved_under_another_node_id_is_rejected() {
        // The hub cannot serve one library's catalog as another's: the node
        // id is in the associated data.
        let key = CatalogKey::generate();
        let blob = key.encrypt(NODE, 1, CATALOG).unwrap();

        assert!(matches!(
            key.decrypt("some-other-node", &blob),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn outdated_key_fails_on_the_key_id_before_decrypting() {
        // After a rotation the ex-reader still holds the old key: the new
        // ciphertext names the new key id and the mismatch is explicit.
        let old_key = CatalogKey::generate();
        let new_key = CatalogKey::generate();
        let blob = new_key.encrypt(NODE, 2, CATALOG).unwrap();

        assert!(matches!(
            old_key.decrypt(NODE, &blob),
            Err(CryptoError::KeyIdMismatch)
        ));
        assert_eq!(key_id_of_blob(&blob).unwrap(), new_key.key_id_hex());
    }

    #[test]
    fn padding_hides_the_exact_size() {
        let key = CatalogKey::generate();
        let small = key.encrypt(NODE, 1, b"[]").unwrap();
        let bigger = key.encrypt(NODE, 1, &vec![b'x'; 600]).unwrap();

        // Both fit the first bucket: same ciphertext length.
        assert_eq!(small.len(), bigger.len());
    }

    #[test]
    fn wrap_unwrap_roundtrip_and_wrong_reader_fails() {
        let key = CatalogKey::generate();
        let (reader_secret, reader_public) = reader();
        let (stranger_secret, _) = reader();

        let wrap = key.wrap_for(&reader_public).unwrap();
        let unwrapped = CatalogKey::unwrap(&reader_secret, &wrap).unwrap();

        assert_eq!(unwrapped.key_id, key.key_id);
        assert_eq!(*unwrapped.export_secret(), *key.export_secret());
        assert!(CatalogKey::unwrap(&stranger_secret, &wrap).is_err());
    }

    #[test]
    fn a_reader_with_its_wrap_opens_the_catalog() {
        let key = CatalogKey::generate();
        let (reader_secret, reader_public) = reader();
        let blob = key.encrypt(NODE, 3, CATALOG).unwrap();
        let wrap = key.wrap_for(&reader_public).unwrap();

        let reader_key = CatalogKey::unwrap(&reader_secret, &wrap).unwrap();
        let (version, plaintext) = reader_key.decrypt(NODE, &blob).unwrap();

        assert_eq!(version, 3);
        assert_eq!(plaintext, CATALOG);
    }

    #[test]
    fn rotation_leaves_the_ex_reader_without_a_usable_key() {
        let (ex_secret, ex_public) = reader();
        let (kept_secret, kept_public) = reader();
        let old_key = CatalogKey::generate();
        let ex_wrap = old_key.wrap_for(&ex_public).unwrap();

        let new_key = CatalogKey::generate();
        let blob = new_key.encrypt(NODE, 4, CATALOG).unwrap();
        let kept_wrap = new_key.wrap_for(&kept_public).unwrap();

        let ex_key = CatalogKey::unwrap(&ex_secret, &ex_wrap).unwrap();
        assert!(matches!(
            ex_key.decrypt(NODE, &blob),
            Err(CryptoError::KeyIdMismatch)
        ));
        let kept_key = CatalogKey::unwrap(&kept_secret, &kept_wrap).unwrap();
        assert_eq!(kept_key.decrypt(NODE, &blob).unwrap().1, CATALOG);
    }

    #[test]
    fn hash_is_keyed_stable_and_lowercase_hex() {
        let key = CatalogKey::generate();
        let other = CatalogKey::generate();
        let h1 = key.catalog_hash("[]", "[{\"isbn\":\"1\"}]", 1);
        let h2 = key.catalog_hash("[]", "[{\"isbn\":\"1\"}]", 1);

        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(
            h1.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(h1, other.catalog_hash("[]", "[{\"isbn\":\"1\"}]", 1));
        assert_ne!(h1, key.catalog_hash("[]", "[{\"isbn\":\"1\"}]", 2));
    }

    #[test]
    fn oversized_plaintext_is_refused_on_both_sides() {
        let key = CatalogKey::generate();
        let huge = vec![0u8; MAX_PLAINTEXT_LEN + 1];
        assert!(key.encrypt(NODE, 1, &huge).is_err());

        // A ciphertext built past the bound (compressed zeros) must not be
        // inflated by the reader either.
        let compressed = zstd::stream::encode_all(&huge[..], ZSTD_LEVEL).unwrap();
        assert!(matches!(
            decompress_bounded(&compressed),
            Err(CryptoError::Compression(_))
        ));
    }

    #[test]
    fn garbage_is_not_a_catalog_ciphertext() {
        let key = CatalogKey::generate();
        assert!(key.decrypt(NODE, "not base64!").is_err());
        assert!(key.decrypt(NODE, &BASE64.encode(b"too short")).is_err());
        assert!(key_id_of_blob(&BASE64.encode([0u8; 64])).is_err());
    }

    #[test]
    fn debug_never_prints_the_key() {
        let key = CatalogKey::generate();
        let printed = format!("{key:?}");
        assert!(printed.contains("[REDACTED]"));
        assert!(!printed.contains(&hex::encode(*key.export_secret())));
    }
}
