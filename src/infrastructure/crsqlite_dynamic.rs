//! Dynamic cr-sqlite extension path (ADR-044).
//!
//! The `crsqlite` feature loads cr-sqlite from a dynamic library at runtime, as
//! opposed to the static ship link (`crsqlite-static`, see
//! [`crsqlite_static`](super::crsqlite_static)). This module resolves the path to
//! that library so the pool builder can name it in
//! `SqliteConnectOptions::extension_with_entrypoint`.
//!
//! Two load paths share this module:
//! - macOS dev/test: the vendored `vendor/crsqlite/crsqlite.dylib` from the source
//!   tree, resolved at compile time.
//! - Windows ship: `crsqlite.dll` next to the running executable, resolved at
//!   runtime. Windows allows runtime extension loading; the static-link constraint
//!   driving the other ships comes from iOS (ADR-044), and the symbol-localization
//!   relink that makes the static archive safe (`vendor/crsqlite/vendor-static.sh`)
//!   is Unix-only, so Windows ships the dynamic path instead.
//!
//! It lives in `infrastructure` (next to `crsqlite_static`) so the database
//! bootstrap can reach it without `infrastructure` depending upward on `services`.

/// Resolve `crsqlite.dll` for the shipped Windows app.
///
/// TWO processes open the same database: the Flutter runner (which hosts the
/// `rust_lib_app.dll` FFI library, at the install root) and the bundled backend
/// (`backend\bibliogenius.exe`, in a subfolder). Each process resolves the dll in
/// its OWN executable directory, and the installer ships one copy of the dll in
/// EACH location. Duplicating ~1 MB is more robust than a shared-path lookup that
/// breaks whenever the install tree moves.
#[cfg(target_os = "windows")]
pub(crate) fn vendored_extension_path() -> String {
    match std::env::current_exe() {
        Ok(exe) => dll_next_to(&exe),
        // Fail closed: without the executable path there is no trusted directory
        // to resolve against, and a bare "crsqlite.dll" would defer to the OS
        // loader search path (a DLL-planting surface). A name that cannot exist
        // fails the extension load, surfacing an honest pool-open error instead.
        Err(e) => {
            tracing::error!("cannot resolve crsqlite.dll next to the executable: {e}");
            "crsqlite.dll-unresolved-exe-path".to_string()
        }
    }
}

/// Path to the vendored cr-sqlite dynamic library in the source tree. This is the
/// desktop dev/test path (macOS): the compile-time location only exists on the
/// machine that built the crate. Shipped apps either link cr-sqlite statically
/// (`crsqlite-static`) or, on Windows, use the runtime resolution above.
#[cfg(not(target_os = "windows"))]
pub(crate) fn vendored_extension_path() -> String {
    format!(
        "{}/vendor/crsqlite/crsqlite.dylib",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// `crsqlite.dll` sitting in the same directory as `exe`. Kept cross-platform so
/// the Windows resolution stays unit-testable from the macOS dev machine (the only
/// Windows build in the loop is CI).
#[cfg(any(target_os = "windows", test))]
fn dll_next_to(exe: &std::path::Path) -> String {
    exe.with_file_name("crsqlite.dll")
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::dll_next_to;
    use std::path::{Path, PathBuf};

    // Both shipped Windows processes (Flutter runner at the install root, bundled
    // backend in backend\) must find the dll copy sitting in their own directory.
    // Asserted on path components, not on a literal string: the separator that
    // `with_file_name` inserts is platform-specific, and this test must hold on
    // the macOS dev machine as well as on an eventual Windows checkout.
    #[test]
    fn dll_resolves_next_to_the_executable() {
        for exe in [
            "C:/Program Files/BiblioGenius/bibliogenius.exe",
            "C:/Program Files/BiblioGenius/backend/bibliogenius.exe",
        ] {
            let exe = Path::new(exe);
            let dll = PathBuf::from(dll_next_to(exe));
            assert_eq!(dll.file_name().unwrap(), "crsqlite.dll");
            assert_eq!(dll.parent(), exe.parent());
        }
    }
}
