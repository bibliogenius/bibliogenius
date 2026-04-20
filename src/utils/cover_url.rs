//! Single source of truth for "what is a servable cover URL".
//!
//! Before this module, four call sites (two Rust, two Flutter) each
//! reimplemented the decision. Adding cache-busting, rewriting for
//! relay, or signing a URL meant synchronising the same rule in four
//! places; missing one site produced silent inconsistencies (relay
//! payloads carrying unreachable `/api` paths, peers falling back to
//! OpenLibrary URLs the owner never chose).
//!
//! The Rust side is now centralised here. `models::Book` exposes thin
//! wrappers so the API contract with callers (api/books.rs,
//! api/e2ee.rs, api/peer.rs, api/frb.rs) is unchanged.

use std::fmt;

/// Error raised when a cover URL rewrite intended for a relay-bound
/// payload cannot produce a remotely reachable URL: the source is a
/// local filesystem path and the hub prefix is missing.
///
/// The caller decides whether to abort the payload or strip the
/// offending entries to `None` (see `safe_cover_url_for_relay` and
/// `rewrite_cover_urls_for_relay` in `models::Book`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverResolveError {
    pub book_ids: Vec<i32>,
}

impl fmt::Display for CoverResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cover rewrite requires a hub prefix but none is configured (book_ids: {:?})",
            self.book_ids
        )
    }
}

impl std::error::Error for CoverResolveError {}

/// Scope of the resolution.
///
/// - `Lan`: callers that serve the payload over local HTTP (same-network
///   peers can resolve a relative `/api/books/{id}/cover` path). A local
///   filesystem path without a hub prefix falls back to that relative
///   URL.
/// - `Relay`: callers that send the payload through the hub relay to a
///   peer with no direct HTTP route back. The `/api/...` fallback is
///   unreachable in that context, so a local path without hub prefix is
///   an error the caller must handle explicitly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResolveScope {
    Lan,
    Relay,
}

/// True when `url` is directly fetchable by any peer over the Internet,
/// regardless of LAN topology. Matches security rule S5 ("no local file
/// paths in hub catalog data"): every URL this returns `true` for is
/// safe to embed in a payload pushed to the hub.
pub fn is_servable_remotely(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// True when `url` is either servable remotely or a well-formed `/api/`
/// relative path. LAN peers can resolve the latter against the owner's
/// base URL; relay peers cannot.
pub fn is_servable_on_lan(url: &str) -> bool {
    is_servable_remotely(url) || url.starts_with("/api")
}

/// Strips non-alphanumeric characters from a timestamp so it can ride
/// in a `?v=` query parameter without percent-encoding. A SQLite
/// timestamp `"2026-04-20 10:30:00"` becomes `"20260420103000"` —
/// deterministic, short, and changes on every edit.
fn version_tag(updated_at: &str) -> String {
    updated_at
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// Appends a `?v={tag}` cache-buster derived from `updated_at` to
/// `base`. No-op when `updated_at` is `None`, empty, or strips to an
/// empty tag. The peer's image cache (`CachedNetworkImage` on Flutter,
/// the hub's cover endpoint on the Rust side) uses the full URL as
/// cache key, so bumping `updated_at` triggers a refetch without
/// waiting for the 7-day TTL.
pub fn append_version(base: String, updated_at: Option<&str>) -> String {
    match updated_at {
        Some(s) if !s.is_empty() => {
            let tag = version_tag(s);
            if tag.is_empty() {
                base
            } else {
                format!("{base}?v={tag}")
            }
        }
        _ => base,
    }
}

fn build_hub_url(hub_cover_prefix: &str, book_id: i32, updated_at: Option<&str>) -> String {
    append_version(format!("{hub_cover_prefix}/{book_id}"), updated_at)
}

fn build_lan_url(book_id: i32, updated_at: Option<&str>) -> String {
    append_version(format!("/api/books/{book_id}/cover"), updated_at)
}

/// Resolve a single cover URL to its final remotely-fetchable form.
///
/// - `None` in, `None` out.
/// - HTTP(S) URLs and `/api` paths pass through untouched.
/// - Local filesystem paths are rewritten to a hub URL when the hub is
///   configured, or to `/api/books/{id}/cover` in LAN scope.
/// - In `Relay` scope, a local path without hub prefix returns
///   `CoverResolveError` so the caller can decide whether to strip or
///   abort.
///
/// `updated_at` (if any) appends the canonical `?v={tag}` cache-buster
/// so peers refetch after re-uploads.
pub fn resolve_single(
    cover_url: Option<&str>,
    book_id: i32,
    updated_at: Option<&str>,
    hub_cover_prefix: Option<&str>,
    scope: ResolveScope,
) -> Result<Option<String>, CoverResolveError> {
    match cover_url {
        None => Ok(None),
        Some(url) if is_servable_on_lan(url) => Ok(Some(url.to_string())),
        Some(_) => match hub_cover_prefix {
            Some(prefix) => Ok(Some(build_hub_url(prefix, book_id, updated_at))),
            None => match scope {
                ResolveScope::Lan => Ok(Some(build_lan_url(book_id, updated_at))),
                ResolveScope::Relay => Err(CoverResolveError {
                    book_ids: vec![book_id],
                }),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // is_servable_remotely / is_servable_on_lan ---------------------------

    #[test]
    fn servable_remotely_accepts_http_and_https_only() {
        assert!(is_servable_remotely("http://a"));
        assert!(is_servable_remotely("https://a"));
        assert!(!is_servable_remotely("/api/books/1/cover"));
        assert!(!is_servable_remotely("/var/mobile/c.jpg"));
        assert!(!is_servable_remotely(""));
        assert!(!is_servable_remotely("file:///x"));
    }

    #[test]
    fn servable_on_lan_accepts_http_and_api_relative() {
        assert!(is_servable_on_lan("https://cdn/x.jpg"));
        assert!(is_servable_on_lan("/api/books/1/cover"));
        assert!(!is_servable_on_lan("/var/mobile/c.jpg"));
        assert!(!is_servable_on_lan(""));
    }

    // append_version ------------------------------------------------------

    #[test]
    fn append_version_strips_non_alnum() {
        assert_eq!(
            append_version("https://h/c/7".into(), Some("2026-04-20 10:30:00")),
            "https://h/c/7?v=20260420103000"
        );
    }

    #[test]
    fn append_version_noop_when_missing_or_empty() {
        assert_eq!(append_version("base".into(), None), "base");
        assert_eq!(append_version("base".into(), Some("")), "base");
        // A timestamp that strips to empty must not emit a dangling `?v=`.
        assert_eq!(append_version("base".into(), Some("----")), "base");
    }

    // resolve_single ------------------------------------------------------

    #[test]
    fn resolve_single_none_passthrough() {
        let out = resolve_single(None, 1, None, None, ResolveScope::Relay).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn resolve_single_http_passthrough() {
        let out = resolve_single(
            Some("https://cdn/ok.jpg"),
            1,
            None,
            None,
            ResolveScope::Relay,
        )
        .unwrap();
        assert_eq!(out.as_deref(), Some("https://cdn/ok.jpg"));
    }

    #[test]
    fn resolve_single_api_passthrough() {
        let out = resolve_single(
            Some("/api/books/2/cover"),
            2,
            None,
            None,
            ResolveScope::Relay,
        )
        .unwrap();
        assert_eq!(out.as_deref(), Some("/api/books/2/cover"));
    }

    #[test]
    fn resolve_single_local_with_hub_builds_hub_url() {
        let out = resolve_single(
            Some("/var/mobile/c.jpg"),
            42,
            None,
            Some("https://hub/api/directory/n/covers"),
            ResolveScope::Relay,
        )
        .unwrap();
        assert_eq!(
            out.as_deref(),
            Some("https://hub/api/directory/n/covers/42")
        );
    }

    #[test]
    fn resolve_single_local_lan_without_hub_falls_back_to_api() {
        let out =
            resolve_single(Some("/var/mobile/c.jpg"), 7, None, None, ResolveScope::Lan).unwrap();
        assert_eq!(out.as_deref(), Some("/api/books/7/cover"));
    }

    #[test]
    fn resolve_single_local_relay_without_hub_errors() {
        let err = resolve_single(
            Some("/var/mobile/c.jpg"),
            42,
            None,
            None,
            ResolveScope::Relay,
        )
        .unwrap_err();
        assert_eq!(err.book_ids, vec![42]);
    }

    #[test]
    fn resolve_single_appends_version_from_updated_at() {
        let out = resolve_single(
            Some("/var/mobile/c.jpg"),
            42,
            Some("2026-04-20 10:30:00"),
            Some("https://hub/api/directory/n/covers"),
            ResolveScope::Relay,
        )
        .unwrap();
        assert_eq!(
            out.as_deref(),
            Some("https://hub/api/directory/n/covers/42?v=20260420103000")
        );
    }

    #[test]
    fn resolve_single_lan_appends_version_for_local_fallback() {
        let out = resolve_single(
            Some("/var/mobile/c.jpg"),
            7,
            Some("2026-04-20 10:30:00"),
            None,
            ResolveScope::Lan,
        )
        .unwrap();
        assert_eq!(out.as_deref(), Some("/api/books/7/cover?v=20260420103000"));
    }

    /// Security rule S5: a URL that passes `is_servable_remotely` is
    /// safe to embed in hub catalog payloads. The inverse (local path)
    /// must NEVER reach the hub. This guard asserts the predicate
    /// rejects every path shape the project has seen in the wild.
    #[test]
    fn is_servable_remotely_rejects_every_known_local_shape() {
        for bad in [
            "/var/mobile/Containers/Data/Application/abc/Documents/covers/1.jpg",
            "/Users/x/Library/Application Support/com.bibliogenius.app/covers/1.jpg",
            "/data/user/0/com.bibliogenius.app/files/covers/1.jpg",
            "/api/books/1/cover",
            "covers/1.jpg",
            "",
        ] {
            assert!(
                !is_servable_remotely(bad),
                "S5 leak: {bad:?} must not pass is_servable_remotely"
            );
        }
    }
}
