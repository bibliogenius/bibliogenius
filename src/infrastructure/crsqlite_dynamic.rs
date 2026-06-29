//! Dynamic cr-sqlite extension path (dev/test load path, ADR-044).
//!
//! The `crsqlite` feature loads cr-sqlite from a vendored dynamic library at
//! runtime — the desktop dev/test path — as opposed to the static ship link
//! (`crsqlite-static`, see [`crsqlite_static`](super::crsqlite_static)). This
//! module exposes the path to that vendored library so the pool builder can name
//! it in `SqliteConnectOptions::extension_with_entrypoint`.
//!
//! It lives in `infrastructure` (next to `crsqlite_static`) so the database
//! bootstrap can reach it without `infrastructure` depending upward on `services`.

/// Path to the vendored cr-sqlite dynamic library. The shipped app links cr-sqlite
/// statically; this is the local desktop dev/test path that loads the extension at
/// runtime.
pub(crate) fn vendored_extension_path() -> String {
    format!(
        "{}/vendor/crsqlite/crsqlite.dylib",
        env!("CARGO_MANIFEST_DIR")
    )
}
