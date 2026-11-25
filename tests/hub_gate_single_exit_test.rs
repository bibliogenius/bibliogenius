//! The hub gate (ADR-073) holds only as long as requests to the hub keep
//! leaving through the gated functions. These checks read the sources: a new
//! sender written beside the gate fails here, before any review.
//!
//! Production code only: each file is cut at its test module, and comment
//! lines are ignored.

use std::path::{Path, PathBuf};

fn production_code(path: &Path) -> String {
    let source = std::fs::read_to_string(path).expect("read source");
    // The test module, not a lone `#[cfg(test)]` helper in the middle of a file.
    let production = match source.find("#[cfg(test)]\nmod ") {
        Some(start) => &source[..start],
        None => source.as_str(),
    };
    production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn rust_sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// `(file relative to src/, occurrences)` of `needle` in production code.
fn occurrences(needle: &str) -> Vec<(String, usize)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    let mut hits: Vec<(String, usize)> = files
        .iter()
        // Generated bindings, and test-only modules (declared `#[cfg(test)]`
        // in their parent, so the cut above does not see them).
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name != "frb_generated.rs"
                && !name.ends_with("_tests.rs")
                && !name.contains("test_support")
        })
        .filter_map(|path| {
            let count = production_code(path).matches(needle).count();
            let relative = path
                .strip_prefix(&src)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            (count > 0).then_some((relative, count))
        })
        .collect();
    hits.sort();
    hits
}

fn expect(needle: &str, allowed: &[(&str, usize)], why: &str) {
    let mut expected: Vec<(String, usize)> = allowed
        .iter()
        .map(|(file, count)| ((*file).to_string(), *count))
        .collect();
    expected.sort();
    assert_eq!(occurrences(needle), expected, "`{needle}` moved. {why}");
}

fn count_in(file: &str, needle: &str) -> usize {
    occurrences(needle)
        .into_iter()
        .find(|(name, _)| name == file)
        .map_or(0, |(_, count)| count)
}

#[test]
fn the_relay_transport_sends_from_one_place() {
    assert_eq!(
        count_in("services/relay_transport.rs", ".send()"),
        1,
        "every relay request goes through `dispatch`, behind the hub gate"
    );
    assert_eq!(
        count_in("services/relay_transport.rs", "hub_gate::ensure_open()"),
        1,
        "`dispatch` checks the hub gate"
    );
}

#[test]
fn relay_mailbox_urls_are_built_in_the_transport_only() {
    // `api/relay.rs` is the relay server of this core (its own routes), not
    // a client of the hub: its paths live in doc comments only.
    expect(
        "/api/relay/mailbox",
        &[("services/relay_transport.rs", 7)],
        "A request to a relay mailbox is built in relay_transport.rs, where the hub gate is checked.",
    );
}

#[test]
fn the_account_client_sends_from_one_place() {
    assert_eq!(
        count_in("services/account_sync_client.rs", ".send()"),
        1,
        "every account request goes through `send_json`, behind the hub gate"
    );
    assert_eq!(
        count_in("services/account_sync_client.rs", "hub_gate::ensure_open()"),
        1,
        "`send_json` checks the hub gate"
    );
}

#[test]
fn the_hub_address_is_read_from_the_environment_in_known_places_only() {
    expect(
        "env::var(\"HUB_URL\")",
        &[
            // The account client: every request goes through `send_json`.
            ("services/account_sync_client.rs", 1),
            // `hub_base_url`, the reader `hub_base_url_for_request` gates.
            ("services/hub_directory_service.rs", 1),
            // Startup configuration, read by nobody who sends.
            ("infrastructure/config.rs", 1),
            // Builds the address of a cover image, fetched by the app.
            ("models/book.rs", 1),
        ],
        "A request to the hub takes its address from `HubDirectoryService::hub_base_url_for_request`, which checks the hub gate.",
    );
}

#[test]
fn the_ungated_reader_of_the_hub_address_is_used_to_compare_only() {
    expect(
        "hub_base_url()",
        &[
            // `hub_effective_url`: the app keys its Keychain copies on it.
            ("api/frb/lifecycle.rs", 1),
            // `mailbox_on_our_hub`: compares a peer's relay with ours.
            ("api/peer/messaging.rs", 1),
            // `setup_relay`: tells whether the hub changed.
            ("api/peer/relay_config.rs", 1),
            // Its own reader and `AccountSyncClient::new`.
            ("services/account_sync_client.rs", 2),
            // `cached_cover_url`, the definition, and the gated reader.
            ("services/hub_directory_service.rs", 3),
        ],
        "To send a request, use `hub_base_url_for_request` (gated). `hub_base_url` only names the hub.",
    );
}

#[test]
fn the_pass_through_the_closed_gate_has_one_holder() {
    expect(
        "with_withdrawal_pass(",
        &[("services/hub_withdrawal.rs", 1)],
        "Only the withdrawal of the registration sends through a closed gate (ADR-073 D4).",
    );
    assert_eq!(
        count_in("services/hub_gate.rs", "holds_withdrawal_pass()"),
        2,
        "the pass is read by `ensure_open` only (its definition and that one use)"
    );
}

#[test]
fn the_gate_is_closed_until_someone_opens_it() {
    // No test of this file touches the gate: this is its state at start.
    assert!(
        !rust_lib_app::services::hub_gate::is_open(),
        "the hub gate must start closed"
    );
}
