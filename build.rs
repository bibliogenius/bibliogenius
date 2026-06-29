//! Build script: statically link the vendored cr-sqlite archive when the
//! shipping `crsqlite-static` feature is enabled.
//!
//! iOS forbids loading a separate dynamic library, so static linking +
//! in-process registration via `sqlite3_auto_extension` is the cross-platform
//! shipping mechanism for cr-sqlite (ADR-044). This is distinct from
//! the `crsqlite` feature, which loads `crsqlite.dylib` at runtime for the
//! local dev/test path only.
//!
//! The default build enables neither feature and links no native dependency,
//! so this script is a no-op unless `crsqlite-static` is on.

use std::env;
use std::path::PathBuf;

fn main() {
    // Only act for the shipping static-link feature. Cargo sets
    // CARGO_FEATURE_<NAME> for every enabled feature (uppercased, `-` -> `_`).
    if env::var_os("CARGO_FEATURE_CRSQLITE_STATIC").is_none() {
        return;
    }

    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");

    // Vendored archives are named per target. Only platforms whose static
    // archive has been built and checksummed (vendor/crsqlite/CHECKSUMS.txt)
    // are wired here; any other target fails loudly with the build recipe
    // pointer rather than silently linking the wrong artifact.
    let archive = match (arch.as_str(), os.as_str()) {
        ("aarch64", "macos") => "crsqlite-aarch64-apple-darwin.a",
        _ => panic!(
            "crsqlite-static: no vendored cr-sqlite static archive for target \
             {arch}-{os}; build it at the pinned tag per vendor/crsqlite/README.md \
             and add the (arch, os) -> filename mapping here"
        ),
    };

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vendor = manifest.join("vendor").join("crsqlite");
    let src = vendor.join(archive);
    assert!(
        src.exists(),
        "crsqlite-static: vendored archive missing at {} \
         (the .a is gitignored; build it per vendor/crsqlite/README.md)",
        src.display()
    );

    // Supply-chain check: the archive must match the SHA-256 recorded in
    // CHECKSUMS.txt (ADR-044). Catches a corrupted or swapped binary before it
    // is linked into the app.
    verify_checksum(&vendor.join("CHECKSUMS.txt"), &src, archive);

    // rustc's `link-lib=static=crsqlite` expects a file named `libcrsqlite.a`,
    // but the vendored file is target-suffixed. Copy it into OUT_DIR under the
    // canonical name and link from there, so the per-target naming stays in the
    // vendor dir without a symlink in the source tree.
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let dst = out.join("libcrsqlite.a");
    std::fs::copy(&src, &dst).unwrap_or_else(|e| {
        panic!(
            "crsqlite-static: failed to copy {} -> {}: {e}",
            src.display(),
            dst.display()
        )
    });

    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=crsqlite");
}

/// Verify `archive`'s SHA-256 against the entry for `archive_name` in
/// `checksums_path` (lines of `<hex>  <filename>`, the `shasum -a 256` format).
/// Panics on a missing entry or a mismatch.
fn verify_checksum(
    checksums_path: &std::path::Path,
    archive: &std::path::Path,
    archive_name: &str,
) {
    use sha2::{Digest, Sha256};

    println!("cargo:rerun-if-changed={}", checksums_path.display());
    let checksums = std::fs::read_to_string(checksums_path).unwrap_or_else(|e| {
        panic!(
            "crsqlite-static: cannot read {}: {e}",
            checksums_path.display()
        )
    });

    let expected = checksums
        .lines()
        .find_map(|line| {
            let (hex, name) = line.split_once("  ")?;
            (name.trim() == archive_name).then(|| hex.trim().to_ascii_lowercase())
        })
        .unwrap_or_else(|| {
            panic!(
                "crsqlite-static: no SHA-256 entry for {archive_name} in {}",
                checksums_path.display()
            )
        });

    let bytes = std::fs::read(archive)
        .unwrap_or_else(|e| panic!("crsqlite-static: cannot read {}: {e}", archive.display()));
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    assert!(
        actual == expected,
        "crsqlite-static: checksum mismatch for {archive_name}\n  expected {expected}\n  actual   {actual}\n\
         The vendored archive does not match CHECKSUMS.txt; re-vendor from the pinned vlcn release."
    );
}
