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
//! api/e2ee.rs, api/peer/, api/frb/) is unchanged.

use std::fmt;
use std::path::{Path, PathBuf};

/// Error raised when a cover URL rewrite intended for a relay-bound
/// payload cannot produce a remotely reachable URL: the source is a
/// local filesystem path and the hub prefix is missing.
///
/// The caller decides whether to abort the payload or strip the
/// offending entries to `None` (see `safe_cover_url_for_relay` and
/// `rewrite_cover_urls_for_relay` in `models::Book`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverResolveError {
    pub book_ids: Vec<String>,
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

fn build_hub_url(hub_cover_prefix: &str, book_id: &str, updated_at: Option<&str>) -> String {
    append_version(format!("{hub_cover_prefix}/{book_id}"), updated_at)
}

fn build_lan_url(book_id: &str, updated_at: Option<&str>) -> String {
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
    book_id: &str,
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
                    book_ids: vec![book_id.to_string()],
                }),
            },
        },
    }
}

/// Re-bases a stored local cover path onto the current `covers_dir`, keyed by
/// the book's `book_id`.
///
/// The serve-side endpoint (`api/books::get_book_cover`) reads a peer's own
/// covers from disk by the absolute path persisted in `books.cover_url`. On
/// iOS the app's data-container UUID can change across an update, so that
/// absolute path points at a dead container even though the file survives
/// under the new one. This mirrors the Flutter-side `LocalCoverResolver`.
///
/// A device's own custom covers are always named `<book_id>.jpg`, so the
/// re-base uses the current `book_id` — never the stored basename. Guarding on
/// `basename == "<book_id>.jpg"` rebases only this device's own canonical
/// cover; a path carried over from another device (multi-device sync, ADR-011,
/// stores raw paths with the SOURCE id under a fresh local id) has a
/// mismatched basename and is returned untouched, so it is never mapped onto
/// an unrelated local cover. Re-basing onto a single component is also
/// traversal-safe by construction.
///
/// On macOS/Android the data directory is keyed by a fixed bundle id, so the
/// re-based path is identical to the stored one: no behavior change there.
pub fn rebase_local_cover_path(covers_dir: &Path, stored: &str, book_id: &str) -> PathBuf {
    let canonical = format!("{book_id}.jpg");
    match Path::new(stored).file_name() {
        Some(name) if name == canonical.as_str() => covers_dir.join(&canonical),
        _ => PathBuf::from(stored),
    }
}

/// The on-disk path every reader of a book's OWN local cover must go through.
///
/// `covers_dir` is `Some` in app (FFI) mode, where `init_backend` registers the
/// current covers directory, and `None` in server-binary mode, where paths are
/// stable and the stored value is read as-is.
///
/// Every code path that opens a stored `books.cover_url` as a file must resolve
/// it here rather than reading the column raw. The column keeps whatever
/// absolute path the writing device had at the time; on iOS that prefix dies
/// with the next data-container UUID change. A raw read then fails with ENOENT
/// on a file that is still there under the new container, which is invisible in
/// the UI (the Flutter side re-bases too, so the cover still renders) and
/// surfaces only as a downstream failure, such as a hub cover upload that never
/// stops retrying.
pub fn resolve_local_cover_read_path(
    covers_dir: Option<&Path>,
    stored: &str,
    book_id: &str,
) -> PathBuf {
    match covers_dir {
        Some(dir) => rebase_local_cover_path(dir, stored, book_id),
        None => PathBuf::from(stored),
    }
}

/// The on-disk path of a book's OWN local custom cover in app mode:
/// `<covers_dir>/<book_id>.jpg`.
///
/// `books.cover_url` replicates raw across devices (ADR-011), so the stored
/// value is not necessarily one this device wrote: a paired device can put any
/// readable path there. Nothing in app mode may therefore let that value decide
/// where a read, or even a stat, happens. The location is derived from the
/// book's own identity instead, which is exactly where `services/cover_sync.rs`
/// writes and where the upload reads.
///
/// `None` when `book_id` cannot stand as a single path component, so a hostile
/// id can never climb out of the covers directory through `join`. Same allowlist
/// as the `is_safe_uuid` guard in `cover_sync.rs`.
pub fn own_local_cover_path(covers_dir: &Path, book_id: &str) -> Option<PathBuf> {
    if book_id.is_empty()
        || !book_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(covers_dir.join(local_cover_filename(book_id)))
}

/// True when a stored `cover_url` names a local custom cover whose bytes are
/// NOT on this device.
///
/// `books.cover_url` replicates across a user's devices, the file does not: the
/// cover lane carries it separately (ADR-046), so between the row landing and
/// the bytes arriving, a device holds a row claiming a cover it cannot produce.
/// Reading that as "there is a cover here" makes the device advertise a hub URL
/// built from its OWN node id, and the hub registration is device-local, not
/// replicated: only this device's own uploads can ever land under that node. The
/// URL is therefore dead by construction, and a peer caches it for days.
///
/// The presence test derives the path the same way the upload does, so the two
/// can never disagree about whether the bytes are there, and the replicated
/// value is never used to pick what gets stat'd. A book id too hostile to derive
/// from counts as absent: nothing can be advertised for it either way.
/// `None` covers_dir is server-binary mode, which has no covers directory to key
/// the check on and reads stored paths as given: unchanged there.
pub fn local_cover_bytes_absent(covers_dir: Option<&Path>, cover_url: &str, book_id: &str) -> bool {
    match covers_dir {
        None => false,
        Some(dir) => {
            is_local_cover(cover_url)
                && !own_local_cover_path(dir, book_id).is_some_and(|path| path.exists())
        }
    }
}

/// Reduce a stored `cover_url` to its device-independent form for storage.
///
/// The same logical cover must be stored identically on every device so the
/// column can be replicated by field-level LWW without one device clobbering
/// another with a path only valid locally (ADR-044 Addendum A.4). Two value
/// kinds are already device-independent and pass through untouched:
///
/// - HTTP(S) URLs (external covers),
/// - `/api/...` relative paths (peer covers).
///
/// Anything else is treated as a local filesystem path: only the final
/// component (the basename, e.g. `<id>.jpg`) is kept. The absolute prefix
/// (`/var/mobile/Containers/<UUID>/...`) is vestigial — it is recomputed at
/// runtime by `rebase_local_cover_path` and the Flutter `LocalCoverResolver`,
/// so dropping it loses nothing. `None`/empty in, same out.
pub fn normalize_cover_url_for_storage(cover_url: Option<&str>) -> Option<String> {
    match cover_url {
        None => None,
        Some(url) if url.is_empty() || is_servable_on_lan(url) => Some(url.to_string()),
        Some(url) => Path::new(url)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .or_else(|| Some(url.to_string())),
    }
}

/// True when a stored `cover_url` value is a local custom-cover reference (a
/// device's own `<id>.jpg` file), as opposed to an external `http(s)` URL or a
/// `/api/...` peer path. Used by the uuid migration to decide which covers need
/// their on-disk file renamed `<id>.jpg` -> `<uuid>.jpg` (ADR-044 Addendum A.4).
pub fn is_local_cover(cover_url: &str) -> bool {
    !cover_url.is_empty() && !is_servable_on_lan(cover_url)
}

/// The canonical on-disk filename for a book's local custom cover, keyed by its
/// uuid identity: `<uuid>.jpg`. Matches `rebase_local_cover_path`'s expectation
/// and the Flutter `LocalCoverResolver`.
pub fn local_cover_filename(uuid: &str) -> String {
    format!("{uuid}.jpg")
}

/// Plan the `cover_url` migration for one book during the id -> uuid flip (S4d).
///
/// Given the currently-stored value and the book's uuid, returns:
/// - the value to STORE in `cover_url` (`None` only when the input is `None`),
/// - for a local custom cover, the on-disk file rename `(old_basename,
///   new_basename)` the caller applies after the transaction commits.
///
/// Local custom covers (`<old id>.jpg`) are re-keyed to `<uuid>.jpg` so the
/// resolver finds them by the book's new uuid identity. External `http(s)` URLs
/// and `/api/...` peer paths are stored normalized, with no rename. A cover
/// already named `<uuid>.jpg` yields no rename (idempotent).
pub fn plan_cover_migration(
    current: Option<&str>,
    uuid: &str,
) -> (Option<String>, Option<(String, String)>) {
    let Some(normalized) = normalize_cover_url_for_storage(current) else {
        return (None, None);
    };
    if is_local_cover(&normalized) {
        let new_name = local_cover_filename(uuid);
        let rename = (normalized != new_name).then(|| (normalized.clone(), new_name.clone()));
        (Some(new_name), rename)
    } else {
        (Some(normalized), None)
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
        let out = resolve_single(None, "1", None, None, ResolveScope::Relay).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn resolve_single_http_passthrough() {
        let out = resolve_single(
            Some("https://cdn/ok.jpg"),
            "1",
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
            "2",
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
            "42",
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
        let out = resolve_single(
            Some("/var/mobile/c.jpg"),
            "7",
            None,
            None,
            ResolveScope::Lan,
        )
        .unwrap();
        assert_eq!(out.as_deref(), Some("/api/books/7/cover"));
    }

    #[test]
    fn resolve_single_local_relay_without_hub_errors() {
        let err = resolve_single(
            Some("/var/mobile/c.jpg"),
            "42",
            None,
            None,
            ResolveScope::Relay,
        )
        .unwrap_err();
        assert_eq!(err.book_ids, vec!["42".to_string()]);
    }

    #[test]
    fn resolve_single_appends_version_from_updated_at() {
        let out = resolve_single(
            Some("/var/mobile/c.jpg"),
            "42",
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
            "7",
            Some("2026-04-20 10:30:00"),
            None,
            ResolveScope::Lan,
        )
        .unwrap();
        assert_eq!(out.as_deref(), Some("/api/books/7/cover?v=20260420103000"));
    }

    // rebase_local_cover_path ---------------------------------------------

    #[test]
    fn rebase_rewrites_stale_own_path_when_basename_matches_id() {
        let covers = Path::new(
            "/var/mobile/Containers/Data/Application/NEW-UUID/Library/Application Support/covers",
        );
        let stored = "/var/mobile/Containers/Data/Application/OLD-UUID/Library/Application Support/covers/42.jpg";
        assert_eq!(
            rebase_local_cover_path(covers, stored, "42"),
            covers.join("42.jpg")
        );
    }

    #[test]
    fn rebase_does_not_remap_a_foreign_synced_path() {
        // Book row 87 carries device A's path (basename 42). Must NOT map onto
        // this device's 42.jpg — that would show an unrelated book's cover.
        let covers = Path::new("/now/covers");
        let stored = "/var/mobile/.../Application Support/covers/42.jpg";
        assert_eq!(
            rebase_local_cover_path(covers, stored, "87"),
            Path::new(stored)
        );
    }

    #[test]
    fn rebase_rewrites_a_bare_basename_matching_id() {
        let covers = Path::new("/now/covers");
        assert_eq!(
            rebase_local_cover_path(covers, "42.jpg", "42"),
            covers.join("42.jpg")
        );
    }

    #[test]
    fn rebase_is_a_noop_when_already_in_covers_dir() {
        let covers = Path::new("/Users/x/Application Support/covers");
        let stored = "/Users/x/Application Support/covers/7.jpg";
        assert_eq!(
            rebase_local_cover_path(covers, stored, "7"),
            Path::new(stored)
        );
    }

    // resolve_local_cover_read_path ---------------------------------------

    #[test]
    fn resolve_rebases_a_dead_container_path_when_the_covers_dir_is_known() {
        let covers = Path::new("/now/covers");
        let stored = "/var/mobile/Containers/Data/Application/OLD-UUID/Library/Application Support/covers/42.jpg";
        assert_eq!(
            resolve_local_cover_read_path(Some(covers), stored, "42"),
            covers.join("42.jpg")
        );
    }

    #[test]
    fn resolve_reads_the_stored_path_as_is_in_server_binary_mode() {
        let stored = "/srv/covers/42.jpg";
        assert_eq!(
            resolve_local_cover_read_path(None, stored, "42"),
            Path::new(stored)
        );
    }

    #[test]
    fn resolve_leaves_a_foreign_synced_path_alone() {
        // Same guard as `rebase_local_cover_path`: a path replicated from
        // another device carries that device's basename, so re-basing it would
        // serve an unrelated book's cover.
        let covers = Path::new("/now/covers");
        let stored = "/var/mobile/.../covers/42.jpg";
        assert_eq!(
            resolve_local_cover_read_path(Some(covers), stored, "87"),
            Path::new(stored)
        );
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

    // normalize_cover_url_for_storage -------------------------------------

    #[test]
    fn normalize_storage_keeps_device_independent_values() {
        // None / empty pass through.
        assert_eq!(normalize_cover_url_for_storage(None), None);
        assert_eq!(
            normalize_cover_url_for_storage(Some("")).as_deref(),
            Some("")
        );
        // External URLs and /api relative paths are already device-independent.
        assert_eq!(
            normalize_cover_url_for_storage(Some("https://cdn/x.jpg")).as_deref(),
            Some("https://cdn/x.jpg")
        );
        assert_eq!(
            normalize_cover_url_for_storage(Some("http://h/c/7")).as_deref(),
            Some("http://h/c/7")
        );
        assert_eq!(
            normalize_cover_url_for_storage(Some("/api/books/2/cover")).as_deref(),
            Some("/api/books/2/cover")
        );
    }

    #[test]
    fn normalize_storage_strips_local_absolute_prefix_to_basename() {
        for path in [
            "/var/mobile/Containers/Data/Application/abc/Documents/covers/1.jpg",
            "/Users/x/Library/Application Support/com.bibliogenius.app/covers/1.jpg",
            "/data/user/0/com.bibliogenius.app/files/covers/1.jpg",
            "covers/1.jpg",
        ] {
            assert_eq!(
                normalize_cover_url_for_storage(Some(path)).as_deref(),
                Some("1.jpg"),
                "expected basename for {path:?}"
            );
        }
        // A bare basename is already normalized (the `<id>.jpg` -> `<uuid>.jpg`
        // rename of the file itself is a separate cover-transport step).
        assert_eq!(
            normalize_cover_url_for_storage(Some("1.jpg")).as_deref(),
            Some("1.jpg")
        );
    }

    // is_local_cover / local_cover_filename -------------------------------

    #[test]
    fn is_local_cover_only_for_device_local_files() {
        // Local custom covers (bare basename or absolute path) are local.
        assert!(is_local_cover("42.jpg"));
        assert!(is_local_cover(
            "/var/mobile/Containers/Data/Application/abc/covers/42.jpg"
        ));
        // External URLs and /api peer paths are NOT local.
        assert!(!is_local_cover("https://cdn/x.jpg"));
        assert!(!is_local_cover("http://h/c/7"));
        assert!(!is_local_cover("/api/books/2/cover"));
        // Empty is not a cover to rename.
        assert!(!is_local_cover(""));
    }

    #[test]
    fn local_cover_filename_is_uuid_dot_jpg() {
        assert_eq!(
            local_cover_filename("0190f5a2-1234-7abc-8def-0123456789ab"),
            "0190f5a2-1234-7abc-8def-0123456789ab.jpg"
        );
    }

    // plan_cover_migration ------------------------------------------------

    const U: &str = "0190f5a2-1234-7abc-8def-0123456789ab";

    #[test]
    fn plan_rekeys_local_absolute_path_and_schedules_rename() {
        // A device-local custom cover stored as an absolute path: re-keyed to
        // <uuid>.jpg, and its on-disk file renamed from the old <id>.jpg.
        let (stored, rename) = plan_cover_migration(
            Some("/var/mobile/Containers/Data/Application/abc/covers/42.jpg"),
            U,
        );
        assert_eq!(
            stored.as_deref(),
            Some("0190f5a2-1234-7abc-8def-0123456789ab.jpg")
        );
        assert_eq!(
            rename,
            Some((
                "42.jpg".to_string(),
                "0190f5a2-1234-7abc-8def-0123456789ab.jpg".to_string()
            ))
        );
    }

    #[test]
    fn plan_rekeys_bare_basename() {
        let (stored, rename) = plan_cover_migration(Some("7.jpg"), U);
        assert_eq!(
            stored.as_deref(),
            Some("0190f5a2-1234-7abc-8def-0123456789ab.jpg")
        );
        assert_eq!(
            rename,
            Some((
                "7.jpg".to_string(),
                "0190f5a2-1234-7abc-8def-0123456789ab.jpg".to_string()
            ))
        );
    }

    #[test]
    fn plan_leaves_external_url_untouched() {
        let (stored, rename) = plan_cover_migration(Some("https://cdn/x.jpg"), U);
        assert_eq!(stored.as_deref(), Some("https://cdn/x.jpg"));
        assert_eq!(rename, None);
    }

    #[test]
    fn plan_leaves_api_peer_path_untouched() {
        let (stored, rename) = plan_cover_migration(Some("/api/books/9/cover"), U);
        assert_eq!(stored.as_deref(), Some("/api/books/9/cover"));
        assert_eq!(rename, None);
    }

    #[test]
    fn plan_is_idempotent_when_already_uuid_named() {
        // A cover already named <uuid>.jpg keeps its value and needs no rename.
        let already = format!("{U}.jpg");
        let (stored, rename) = plan_cover_migration(Some(&already), U);
        assert_eq!(stored.as_deref(), Some(already.as_str()));
        assert_eq!(rename, None);
    }

    #[test]
    fn plan_passes_none_through() {
        let (stored, rename) = plan_cover_migration(None, U);
        assert_eq!(stored, None);
        assert_eq!(rename, None);
    }

    // local_cover_bytes_absent -------------------------------------------

    /// The row replicates, the file does not: a device can hold a book whose
    /// `cover_url` names a custom cover it has never received (ADR-046 carries
    /// the bytes separately). Advertising it would build a hub URL under this
    /// device's own node, where only this device's uploads land.
    #[test]
    fn absent_local_cover_is_reported_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(local_cover_bytes_absent(
            Some(dir.path()),
            &format!("{U}.jpg"),
            U
        ));
    }

    #[test]
    fn present_local_cover_is_not_reported_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let name = format!("{U}.jpg");
        std::fs::write(dir.path().join(&name), b"jpeg").expect("write cover");
        assert!(!local_cover_bytes_absent(Some(dir.path()), &name, U));
    }

    /// The stale absolute prefix the column carries from whichever device wrote
    /// it must not make a cover that IS on disk look missing.
    #[test]
    fn a_stale_stored_prefix_does_not_hide_a_present_cover() {
        let dir = tempfile::tempdir().expect("temp dir");
        let name = format!("{U}.jpg");
        std::fs::write(dir.path().join(&name), b"jpeg").expect("write cover");
        let stale = format!("/var/mobile/Containers/Data/Application/dead/covers/{name}");
        assert!(!local_cover_bytes_absent(Some(dir.path()), &stale, U));
    }

    /// The mirror of `an_absolute_path_outside_the_covers_dir_is_never_read` on
    /// the read side. `cover_url` replicates raw (ADR-011), so a paired device
    /// can point it at any readable file. That file is not this book's cover:
    /// the upload derives its path and will never send those bytes, so treating
    /// the decoy's existence as "the cover is here" would advertise a hub URL
    /// nothing ever backs, and would stat an attacker-chosen path on the way.
    #[test]
    fn a_readable_decoy_outside_the_covers_dir_is_still_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let decoy = dir.path().join("decoy.jpg");
        std::fs::write(&decoy, b"jpeg").expect("write decoy");

        assert!(local_cover_bytes_absent(
            Some(dir.path()),
            decoy.to_str().expect("utf-8 path"),
            U
        ));
    }

    /// A book id that cannot be a path component has no derivable cover, so it
    /// can never be advertised. Guards `join` against a hostile replicated id.
    #[test]
    fn an_unsafe_book_id_has_no_own_cover_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(own_local_cover_path(dir.path(), "../../etc/passwd"), None);
        assert_eq!(own_local_cover_path(dir.path(), ""), None);
        assert!(local_cover_bytes_absent(
            Some(dir.path()),
            "cover.jpg",
            "../../etc/passwd"
        ));
    }

    /// Values that are not local covers are never "missing": they need no file.
    #[test]
    fn remote_and_peer_urls_are_never_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(!local_cover_bytes_absent(
            Some(dir.path()),
            "https://covers.example/x.jpg",
            U
        ));
        assert!(!local_cover_bytes_absent(
            Some(dir.path()),
            "/api/books/42/cover",
            U
        ));
        assert!(!local_cover_bytes_absent(Some(dir.path()), "", U));
    }

    /// Server-binary mode has no covers directory to key the check on and reads
    /// stored paths as given, so it keeps its behavior.
    #[test]
    fn no_covers_dir_never_reports_missing() {
        assert!(!local_cover_bytes_absent(None, &format!("{U}.jpg"), U));
    }
}
