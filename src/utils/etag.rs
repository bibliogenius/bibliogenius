//! ETag helpers for conditional responses.
//!
//! The catalog sync is hot: 5G peers pay bandwidth for every `GET /api/books`
//! and every E2EE `book_sync_request`. A strong SHA-256 ETag over the
//! response body lets clients send `If-None-Match` (or a `catalog_hash` field
//! in the E2EE case) and short-circuit to 304 / "unchanged" when nothing
//! changed. Hashing happens after serialization; the saved cost is the
//! network transfer, which dwarfs the CPU cost of the hash.

use sha2::{Digest, Sha256};

/// Compute a strong ETag value (quoted hex SHA-256) over the given bytes.
///
/// Returns a string including the surrounding double-quotes, suitable for
/// emission as an HTTP `ETag` header or comparison against `If-None-Match`.
pub fn strong_etag(bytes: &[u8]) -> String {
    format!("\"{}\"", hex_sha256(bytes))
}

/// Compute an unquoted hex SHA-256 of the given bytes.
///
/// Used where the raw 64-hex-char digest is needed (e.g. in JSON request
/// bodies or as a value for a `catalog_hash` field). For HTTP ETag headers,
/// use [`strong_etag`] which adds the required RFC 7232 quotes.
pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Return true when an `If-None-Match` header value matches [etag].
///
/// Accepts:
/// - `*` wildcard
/// - A single ETag (`"abc123"`)
/// - A comma-separated list (`"abc", W/"def"`)
/// - Weak ETags prefixed `W/` (compared opaquely against the strong form,
///   as required by RFC 7232 §2.3.2 for `If-None-Match`).
pub fn if_none_match_matches(header_value: &str, etag: &str) -> bool {
    header_value.split(',').any(|candidate| {
        let trimmed = candidate.trim();
        if trimmed == "*" {
            return true;
        }
        let normalized = trimmed.strip_prefix("W/").unwrap_or(trimmed);
        normalized == etag
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_is_deterministic_for_same_bytes() {
        assert_eq!(strong_etag(b"hello"), strong_etag(b"hello"));
    }

    #[test]
    fn etag_differs_when_bytes_differ() {
        assert_ne!(strong_etag(b"hello"), strong_etag(b"world"));
    }

    #[test]
    fn etag_is_quoted_hex() {
        let tag = strong_etag(b"x");
        assert!(tag.starts_with('"'));
        assert!(tag.ends_with('"'));
        // SHA-256 = 64 hex chars + 2 quotes = 66.
        assert_eq!(tag.len(), 66);
    }

    #[test]
    fn matches_exact_single_etag() {
        assert!(if_none_match_matches("\"abc\"", "\"abc\""));
    }

    #[test]
    fn does_not_match_different_etag() {
        assert!(!if_none_match_matches("\"abc\"", "\"xyz\""));
    }

    #[test]
    fn wildcard_always_matches() {
        assert!(if_none_match_matches("*", "\"whatever\""));
    }

    #[test]
    fn matches_in_comma_separated_list() {
        assert!(if_none_match_matches(
            "\"old\", \"current\", \"other\"",
            "\"current\""
        ));
    }

    #[test]
    fn matches_weak_etag_against_strong_form() {
        // For If-None-Match, weak and strong etags with the same opaque
        // body are considered equivalent (RFC 7232 §2.3.2).
        assert!(if_none_match_matches("W/\"abc\"", "\"abc\""));
    }

    #[test]
    fn tolerates_whitespace_around_entries() {
        assert!(if_none_match_matches("  \"abc\"  ,  \"def\"  ", "\"def\""));
    }

    #[test]
    fn empty_header_value_does_not_match() {
        assert!(!if_none_match_matches("", "\"abc\""));
    }

    #[test]
    fn hex_sha256_is_unquoted_64_chars() {
        let hex = hex_sha256(b"hello");
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!hex.contains('"'));
    }

    #[test]
    fn strong_etag_wraps_hex_sha256_in_quotes() {
        let hex = hex_sha256(b"payload");
        let etag = strong_etag(b"payload");
        assert_eq!(etag, format!("\"{hex}\""));
    }
}
