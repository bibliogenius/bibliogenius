//! Device-pairing wire protocol for Path B account enrollment (ST-05 Phase F, ADR-045).
//!
//! Two short JSON payloads carried over an **authenticated QR channel** (in-person scan),
//! never relayed raw through the hub:
//!
//! 1. `bg-pair` — shown by the NEW (un-enrolled) device, scanned by an authorized device.
//!    Carries the new device's random `device_id` lane key and its `NodeIdentity`
//!    Ed25519/X25519 public keys (ADR-039). The authorized device seals the trousseau to
//!    the X25519 key and enrolls the device into the signed registry.
//!
//! 2. `bg-sealed` — shown by the authorized device, scanned back by the new device.
//!    Carries the opaque sealed trousseau (encrypted to the new device, hub-relay-safe)
//!    plus the account `email` the new device needs to log in.
//!
//! **BLOCKING INVARIANT (ADR-045 / ADR-042 §14 H2):** the recipient X25519 public key used
//! to seal the trousseau MUST come ONLY from a scanned [`PairingRequest`] — never a hub
//! field or any network response. [`PairingRequest::to_device_entry`] is the single bridge
//! from "scanned bytes" to "the key we seal to"; the FFI layer must call `seal_to_device`
//! with exactly that key. This module is the authenticated boundary; it is unit-tested so
//! a non-`bg-pair` payload (e.g. a peer invite) can never be misread as a pairing request.

use serde::Serialize;

use crate::crypto::device_registry::DeviceEntry;

/// Wire `type` tag for the new-device pairing payload. Distinct from the peer-invite QR
/// (`version`-tagged JSON) so the two flows cannot be cross-fed.
const PAIRING_TYPE: &str = "bg-pair";
/// Wire `type` tag for the sealed-trousseau return payload.
const SEALED_TYPE: &str = "bg-sealed";
/// Current pairing protocol version.
const PAIRING_VERSION: u32 = 1;

#[derive(Debug)]
pub enum PairingError {
    /// Not valid JSON.
    Malformed(String),
    /// Wrong/absent `type` tag (e.g. a peer-invite payload was scanned by mistake).
    WrongType,
    /// Unsupported protocol version.
    UnsupportedVersion(u32),
    /// A public-key / device-id field was missing or not the expected length.
    BadField(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(e) => write!(f, "Malformed pairing payload: {e}"),
            Self::WrongType => write!(f, "Not a device-pairing payload"),
            Self::UnsupportedVersion(v) => write!(f, "Unsupported pairing version {v}"),
            Self::BadField(e) => write!(f, "Invalid pairing field: {e}"),
        }
    }
}

impl std::error::Error for PairingError {}

/// The scanned new-device request: everything an authorized device needs to seal the
/// trousseau and add the device to the registry. All fields originate from the QR scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    pub device_id: String,
    pub ed25519_pk: [u8; 32],
    pub x25519_pk: [u8; 32],
    pub name: String,
}

impl PairingRequest {
    /// The authoritative bridge from scanned bytes to the registry entry / seal target.
    /// The X25519 key returned here is the ONLY key the FFI layer may seal the trousseau
    /// to (ADR-045 invariant) — there is no other source.
    pub fn to_device_entry(&self) -> DeviceEntry {
        DeviceEntry {
            device_id: self.device_id.clone(),
            ed25519_pk: self.ed25519_pk,
            x25519_pk: self.x25519_pk,
            name: self.name.clone(),
        }
    }
}

#[derive(Serialize)]
struct PairingWire {
    #[serde(rename = "type")]
    type_tag: String,
    version: u32,
    device_id: String,
    /// Hex-encoded Ed25519 public key (64 chars).
    ed25519_pk: String,
    /// Hex-encoded X25519 public key (64 chars).
    x25519_pk: String,
    name: String,
}

/// Build the `bg-pair` QR payload shown by the new device.
pub fn build_pairing_qr(
    device_id: &str,
    ed25519_pk: &[u8; 32],
    x25519_pk: &[u8; 32],
    name: &str,
) -> String {
    let wire = PairingWire {
        type_tag: PAIRING_TYPE.to_string(),
        version: PAIRING_VERSION,
        device_id: device_id.to_string(),
        ed25519_pk: hex::encode(ed25519_pk),
        x25519_pk: hex::encode(x25519_pk),
        name: name.to_string(),
    };
    // Infallible: PairingWire is a plain struct of owned strings.
    serde_json::to_string(&wire).unwrap_or_default()
}

/// Parse and validate a scanned `bg-pair` payload. The `type` tag is checked FIRST, so any
/// non-pairing payload (e.g. a version-tagged peer-invite QR) is cleanly rejected as
/// [`PairingError::WrongType`] and can never be coerced into a pairing request.
pub fn parse_pairing_qr(payload: &str) -> Result<PairingRequest, PairingError> {
    let v: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| PairingError::Malformed(e.to_string()))?;
    if v.get("type").and_then(|t| t.as_str()) != Some(PAIRING_TYPE) {
        return Err(PairingError::WrongType);
    }
    let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    if version != PAIRING_VERSION {
        return Err(PairingError::UnsupportedVersion(version));
    }
    Ok(PairingRequest {
        device_id: str_field(&v, "device_id")?,
        ed25519_pk: decode_pk_hex(&str_field(&v, "ed25519_pk")?, "ed25519_pk")?,
        x25519_pk: decode_pk_hex(&str_field(&v, "x25519_pk")?, "x25519_pk")?,
        name: v
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[derive(Serialize)]
struct SealedWire {
    #[serde(rename = "type")]
    type_tag: String,
    version: u32,
    /// Standard-base64 sealed trousseau (opaque, encrypted to the new device).
    sealed: String,
    /// The account email the new device logs in with.
    email: String,
}

/// Build the `bg-sealed` return payload shown by the authorized device.
pub fn build_sealed_qr(sealed_b64: &str, email: &str) -> String {
    let wire = SealedWire {
        type_tag: SEALED_TYPE.to_string(),
        version: PAIRING_VERSION,
        sealed: sealed_b64.to_string(),
        email: email.to_string(),
    };
    serde_json::to_string(&wire).unwrap_or_default()
}

/// Parse a scanned `bg-sealed` return payload into `(sealed_b64, email)`. The sealed blob
/// is NOT decoded here — the crypto layer (`open_device_sealed_bundle`) owns the codec and
/// authenticates it; pre-validating would couple this module to that encoding.
pub fn parse_sealed_qr(payload: &str) -> Result<(String, String), PairingError> {
    let v: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| PairingError::Malformed(e.to_string()))?;
    if v.get("type").and_then(|t| t.as_str()) != Some(SEALED_TYPE) {
        return Err(PairingError::WrongType);
    }
    let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    if version != PAIRING_VERSION {
        return Err(PairingError::UnsupportedVersion(version));
    }
    Ok((str_field(&v, "sealed")?, str_field(&v, "email")?))
}

fn str_field(v: &serde_json::Value, field: &str) -> Result<String, PairingError> {
    v.get(field)
        .and_then(|f| f.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| PairingError::BadField(format!("missing {field}")))
}

fn decode_pk_hex(value: &str, field: &str) -> Result<[u8; 32], PairingError> {
    let bytes = hex::decode(value).map_err(|e| PairingError::BadField(format!("{field}: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| PairingError::BadField(format!("{field} must be 32 bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> ([u8; 32], [u8; 32]) {
        ([7u8; 32], [9u8; 32])
    }

    #[test]
    fn pairing_roundtrip_preserves_keys_and_id() {
        let (ed, x) = keys();
        let qr = build_pairing_qr("dev-lane-id", &ed, &x, "Federico's iPhone");
        let req = parse_pairing_qr(&qr).unwrap();
        assert_eq!(req.device_id, "dev-lane-id");
        assert_eq!(req.ed25519_pk, ed);
        assert_eq!(req.x25519_pk, x);
        assert_eq!(req.name, "Federico's iPhone");
        // The DeviceEntry the registry gets carries exactly the scanned keys.
        let entry = req.to_device_entry();
        assert_eq!(entry.x25519_pk, x);
        assert_eq!(entry.device_id, "dev-lane-id");
    }

    #[test]
    fn a_peer_invite_payload_is_not_a_pairing_request() {
        // ADR-045: the pairing parser must refuse the version-tagged peer-invite QR so
        // an attacker cannot smuggle a substitute X25519 key through the wrong channel.
        let peer_invite = serde_json::json!({
            "version": 2,
            "name": "Some Library",
            "url": "http://192.168.1.10:8000",
            "ed25519_public_key": "a".repeat(64),
            "x25519_public_key": "b".repeat(64),
        })
        .to_string();
        assert!(matches!(
            parse_pairing_qr(&peer_invite),
            Err(PairingError::WrongType)
        ));
    }

    #[test]
    fn wrong_type_and_version_are_rejected() {
        let (ed, x) = keys();
        let good = build_pairing_qr("d", &ed, &x, "n");
        // Tamper the version.
        let bumped = good.replace("\"version\":1", "\"version\":99");
        assert!(matches!(
            parse_pairing_qr(&bumped),
            Err(PairingError::UnsupportedVersion(99))
        ));
        // A bg-sealed payload is not a bg-pair payload.
        let sealed = build_sealed_qr("c2VhbGVk", "r@e.org");
        assert!(matches!(
            parse_pairing_qr(&sealed),
            Err(PairingError::WrongType)
        ));
    }

    #[test]
    fn bad_key_length_is_rejected() {
        let short = serde_json::json!({
            "type": "bg-pair", "version": 1, "device_id": "d",
            "ed25519_pk": "abcd", "x25519_pk": "ef01", "name": "n",
        })
        .to_string();
        assert!(matches!(
            parse_pairing_qr(&short),
            Err(PairingError::BadField(_))
        ));
    }

    #[test]
    fn sealed_roundtrip_and_wrong_type() {
        let qr = build_sealed_qr("c2VhbGVkLWJsb2I", "reader@example.org");
        let (sealed, email) = parse_sealed_qr(&qr).unwrap();
        assert_eq!(sealed, "c2VhbGVkLWJsb2I");
        assert_eq!(email, "reader@example.org");

        // A bg-pair payload must not be read as a sealed-return payload.
        let (ed, x) = keys();
        let pairing = build_pairing_qr("d", &ed, &x, "n");
        assert!(matches!(
            parse_sealed_qr(&pairing),
            Err(PairingError::WrongType)
        ));

        // A missing field is a clean BadField, not a panic.
        let missing =
            serde_json::json!({ "type": "bg-sealed", "version": 1, "email": "x@y.z" }).to_string();
        assert!(matches!(
            parse_sealed_qr(&missing),
            Err(PairingError::BadField(_))
        ));
    }
}
