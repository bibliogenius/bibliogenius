//! Default library name seed produced at migration time.
//!
//! Replaces the legacy literal `"My Library"` seed in `library_config`. The
//! goal is that any process that boots the DB (standalone Rust binary in MCP
//! mode, CLI tools, integration tests, scripts) ends up with a non placeholder
//! name. In FFI mode Flutter still overwrites the seed with its own
//! device-name-aware default; that scenario is unaffected.
//!
//! The format is `"<localized prefix> <host> #<tag>"`, e.g. `"Library of
//! ubuntu-prod #A7K2"`. If the host cannot be detected the host part is
//! dropped and we emit `"Library #<tag>"`.

use rand::Rng;
use unicode_normalization::UnicodeNormalization;

/// Alphabet shared with the Flutter side (theme_provider.dart `generateTag`).
/// Excludes `O`, `0`, `I`, `1`, `L` to avoid visual ambiguity.
const TAG_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
const TAG_LEN: usize = 4;

/// Generate a 4-char tag from `TAG_ALPHABET`.
pub fn generate_tag() -> String {
    let mut rng = rand::thread_rng();
    (0..TAG_LEN)
        .map(|_| TAG_ALPHABET[rng.gen_range(0..TAG_ALPHABET.len())] as char)
        .collect()
}

/// Clean a raw hostname: strip `.local`, drop control characters, NFC
/// normalize, return `None` if the result is empty.
pub fn scrub_hostname(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let without_local = trimmed.strip_suffix(".local").unwrap_or(trimmed);
    let cleaned: String = without_local
        .chars()
        .filter(|c| !c.is_control())
        .nfc()
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Detect the system hostname via the `hostname` crate, then scrub it.
pub fn detect_hostname() -> Option<String> {
    let raw = hostname::get().ok()?;
    let raw = raw.to_string_lossy().into_owned();
    scrub_hostname(&raw)
}

/// Detect the user-facing language from `$LC_ALL` then `$LANG`. Returns
/// `"fr"` if the locale starts with `fr`, otherwise `"en"`.
///
/// Note: in FFI mode on iOS/Android these env vars are usually not set,
/// which is fine because Flutter overwrites the seed anyway. The detection
/// matters mostly for desktop standalone Rust.
pub fn detect_locale_lang() -> &'static str {
    let raw = std::env::var("LC_ALL")
        .ok()
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_default();
    if raw.to_ascii_lowercase().starts_with("fr") {
        "fr"
    } else {
        "en"
    }
}

/// Compose the default library name seed. Idempotent inputs guaranteed only
/// for the `<host>` and `<lang>` parts; the tag is fresh on every call so this
/// MUST only be invoked from the migration seed path. Read-side fallbacks
/// must NOT call this (would yield volatile names).
pub fn compute_default_library_name_seed() -> String {
    let tag = generate_tag();
    match (detect_hostname(), detect_locale_lang()) {
        (Some(host), "fr") => format!("Bibliothèque de {host} #{tag}"),
        (Some(host), _) => format!("Library of {host} #{tag}"),
        (None, _) => format!("Library #{tag}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_tag_has_correct_length_and_alphabet() {
        for _ in 0..32 {
            let tag = generate_tag();
            assert_eq!(tag.len(), TAG_LEN);
            for ch in tag.chars() {
                assert!(
                    TAG_ALPHABET.contains(&(ch as u8)),
                    "char {ch} not in alphabet"
                );
            }
        }
    }

    #[test]
    fn generate_tag_is_not_constant() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..16 {
            seen.insert(generate_tag());
        }
        // 16 draws over ~810k combinations: collisions are vanishingly
        // unlikely. This guards against a stubbed RNG returning constants.
        assert!(seen.len() > 1, "tag generator looks deterministic");
    }

    #[test]
    fn scrub_hostname_strips_dot_local() {
        assert_eq!(scrub_hostname("MacBook.local"), Some("MacBook".to_string()));
    }

    #[test]
    fn scrub_hostname_preserves_internal_dots() {
        assert_eq!(
            scrub_hostname("host.example.com"),
            Some("host.example.com".to_string())
        );
    }

    #[test]
    fn scrub_hostname_normalizes_nfc() {
        let composed = "MacBook-de-Frédéric.local";
        let result = scrub_hostname(composed).unwrap();
        assert_eq!(result, "MacBook-de-Frédéric");
        // NFC normalized: composed e + acute accent should be a single
        // codepoint U+00E9, not e (U+0065) + combining acute (U+0301).
        let bytes = result.as_bytes();
        assert!(
            bytes.windows(2).any(|w| w == [0xC3, 0xA9]),
            "expected NFC-composed é (0xC3 0xA9), got bytes {bytes:?}"
        );
    }

    #[test]
    fn scrub_hostname_rejects_empty_and_control_only() {
        assert_eq!(scrub_hostname(""), None);
        assert_eq!(scrub_hostname("   "), None);
        assert_eq!(scrub_hostname("\x00\x01\x02"), None);
    }

    #[test]
    fn scrub_hostname_drops_control_chars() {
        assert_eq!(
            scrub_hostname("foo\x00bar\x01baz"),
            Some("foobarbaz".to_string())
        );
    }

    #[test]
    fn detect_locale_lang_handles_fr_and_default() {
        // Both cases live in a single test to avoid env-var races between
        // parallel test threads (cargo test runs tests concurrently). All
        // env mutations happen sequentially in this thread.
        //
        // SAFETY: env mutation is unsafe in 2024 edition. We restore the
        // prior values before returning so we don't leak state to other
        // tests that read the env after us.
        let prior_lc_all = std::env::var("LC_ALL").ok();
        let prior_lang = std::env::var("LANG").ok();

        unsafe {
            std::env::remove_var("LC_ALL");
            std::env::remove_var("LANG");
            std::env::set_var("LC_ALL", "fr_FR.UTF-8");
        }
        assert_eq!(detect_locale_lang(), "fr");

        unsafe {
            std::env::remove_var("LC_ALL");
            std::env::remove_var("LANG");
        }
        assert_eq!(detect_locale_lang(), "en");

        unsafe {
            match prior_lc_all {
                Some(v) => std::env::set_var("LC_ALL", v),
                None => std::env::remove_var("LC_ALL"),
            }
            match prior_lang {
                Some(v) => std::env::set_var("LANG", v),
                None => std::env::remove_var("LANG"),
            }
        }
    }

    #[test]
    fn compute_seed_is_never_my_library_and_never_empty() {
        for _ in 0..16 {
            let seed = compute_default_library_name_seed();
            assert!(!seed.trim().is_empty());
            assert_ne!(seed, "My Library");
            assert!(!seed.contains("My Library"));
            // Tag suffix is always present.
            assert!(
                seed.contains(" #"),
                "expected tag separator in seed, got {seed:?}"
            );
        }
    }
}
