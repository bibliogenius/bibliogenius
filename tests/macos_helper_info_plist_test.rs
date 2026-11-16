//! The standalone binary must carry an embedded Info.plist on macOS.
//!
//! The Mac App Store build signs this binary with `com.apple.security.app-sandbox`
//! (Apple requires it on every executable in the bundle). A sandboxed executable
//! that is not inside a bundle of its own gets its identity, and therefore its
//! container, from a `__TEXT,__info_plist` section. Without one, `libsecinit` traps
//! (SIGTRAP, exit 133) before `main` runs, so the MCP helper that an AI assistant
//! spawns from the installed app died silently. `build.rs` embeds the plist at link
//! time; this test guards against that step disappearing.

#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn binary_embeds_a_bundle_identifier() {
    let binary = env!("CARGO_BIN_EXE_bibliogenius");
    let output = Command::new("launchctl")
        .args(["plist", "__TEXT,__info_plist", binary])
        .output()
        .expect("launchctl is available on macOS");
    let dump = String::from_utf8_lossy(&output.stdout);

    assert!(
        dump.contains("\"CFBundleIdentifier\" = \"com.bibliogenius.app.mcp-helper\""),
        "expected an embedded Info.plist with the helper bundle identifier, got:\n{dump}"
    );
}
