//! Natural deduplication keys for the account-sync merge (ST-03).
//!
//! When a user adds the same physical book on two devices, each device mints a
//! *different* `uuid` (see [`crate::utils::uuid_gen`]). On the first account
//! sync (ST-05) those two rows must be recognized as the same book, or the
//! library would double-count it. A globally unique id cannot do that by
//! itself: it makes rows *addressable*, not *correlatable*.
//!
//! This module produces a stable **natural-identity key** from a book's
//! bibliographic fields. The merge in ST-05 compares these keys (scoped to the
//! account's library) to fuse duplicates. The key is a *correlation hint*, not
//! a uniqueness constraint enforced in ST-03.
//!
//! Rule (see also the doc note in `bibliogenius-docs`):
//! - With a usable ISBN: `isbn:<isbn13>`. The ISBN is canonicalized to ISBN-13
//!   (via [`crate::utils::isbn::to_isbn13`]) so an ISBN-10 on one device and
//!   its ISBN-13 form on another still match. Empty / whitespace-only ISBNs
//!   count as absent — the same rule the on-startup dedup uses (migration 057).
//! - Without a usable ISBN: `ta:<title>|<author>|<year>` over normalized
//!   fields. Best effort: ISBN-less books risk *under*-dedup (kept distinct),
//!   which is the safe failure mode — two copies beat a wrong merge.

use crate::utils::isbn;

/// Compute the natural-identity key used by the account merge (ST-05) to detect
/// that two rows (with different uuids, possibly on different devices) describe
/// the same book.
pub fn book_dedup_key(
    isbn: Option<&str>,
    title: &str,
    primary_author: Option<&str>,
    publication_year: Option<i32>,
) -> String {
    if let Some(key) = normalized_isbn(isbn) {
        return format!("isbn:{key}");
    }
    let year = publication_year.map(|y| y.to_string()).unwrap_or_default();
    format!(
        "ta:{}|{}|{}",
        normalize_text(title),
        normalize_text(primary_author.unwrap_or_default()),
        year
    )
}

/// Natural-identity key for a **contact** (ST-05 merge): normalized `email`, else
/// normalized `phone` (digits only), else normalized `name`. Returns `None` when no
/// usable signal exists, so the merge keeps the rows distinct (the safe failure mode).
///
/// Signals are prefixed (`email:`/`phone:`/`name:`) so a phone and a name that happen
/// to normalize to the same string never collide.
pub fn contact_dedup_key(email: Option<&str>, phone: Option<&str>, name: &str) -> Option<String> {
    if let Some(e) = normalized_email(email) {
        return Some(format!("email:{e}"));
    }
    if let Some(p) = normalized_phone(phone) {
        return Some(format!("phone:{p}"));
    }
    let n = normalize_text(name);
    (!n.is_empty()).then(|| format!("name:{n}"))
}

/// Natural-identity key for an **author** (ST-05 merge): normalized `name`, or `None`
/// when the name is empty after normalization.
pub fn author_dedup_key(name: &str) -> Option<String> {
    normalized_name_key(name)
}

/// Natural-identity key for a **tag/shelf** (ST-05 merge): normalized `name`, or `None`
/// when empty. `tags.name` is already locally UNIQUE; this correlates it across devices.
pub fn tag_dedup_key(name: &str) -> Option<String> {
    normalized_name_key(name)
}

/// Shared single-signal name key (author/tag): normalized name, `None` if empty.
fn normalized_name_key(name: &str) -> Option<String> {
    let n = normalize_text(name);
    (!n.is_empty()).then_some(n)
}

/// Lowercased, trimmed email, or `None` when absent/empty. Case is folded (mail
/// addresses are treated case-insensitively); nothing else is rewritten, so a
/// gmail-style dotted alias is an accepted under-dedup miss, never a wrong merge.
fn normalized_email(raw: Option<&str>) -> Option<String> {
    let e = raw?.trim().to_lowercase();
    (!e.is_empty()).then_some(e)
}

/// Digits-only phone, or `None` when absent/empty. Formatting and separators are
/// dropped; international vs national forms (`+33…` vs `0…`) are an accepted miss.
fn normalized_phone(raw: Option<&str>) -> Option<String> {
    let p: String = raw?.chars().filter(|c| c.is_ascii_digit()).collect();
    (!p.is_empty()).then_some(p)
}

/// Canonical ISBN-13 for `raw`, or `None` when absent, empty, or unparseable.
fn normalized_isbn(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    // Canonicalize so isbn10 and isbn13 of the same book correlate.
    if let Some(isbn13) = isbn::to_isbn13(raw) {
        return Some(isbn13);
    }
    // Not a parseable ISBN: keep a stripped, uppercased form so two devices
    // storing the same malformed identifier still match.
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Lowercase, drop punctuation, collapse whitespace. Best-effort normalization
/// for the title/author fallback key.
fn normalize_text(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acceptance criterion: the same book added on two devices (each with its
    /// own uuid) yields the SAME dedup key, so the merge can fuse them.
    #[test]
    fn same_book_on_two_devices_matches() {
        // Device A and device B independently scanned the same ISBN. Their
        // local uuids differ, but the uuid is deliberately not part of the key.
        let device_a = book_dedup_key(
            Some("978-2-264-02484-8"),
            "Martin Eden",
            Some("Jack London"),
            Some(1909),
        );
        let device_b = book_dedup_key(
            Some("9782264024848"),
            "MARTIN EDEN",
            Some("jack london"),
            Some(1909),
        );
        assert_eq!(device_a, device_b);
        assert!(device_a.starts_with("isbn:"));
    }

    #[test]
    fn isbn10_and_isbn13_of_same_book_match() {
        // ISBN-10 and ISBN-13 of the same edition must canonicalize identically.
        let ten = book_dedup_key(Some("2264024844"), "x", None, None);
        let thirteen = book_dedup_key(Some("9782264024848"), "x", None, None);
        assert_eq!(ten, thirteen);
    }

    #[test]
    fn different_isbn_differs() {
        let a = book_dedup_key(Some("9782264024848"), "x", None, None);
        let b = book_dedup_key(Some("9780261103573"), "x", None, None);
        assert_ne!(a, b);
    }

    #[test]
    fn empty_or_whitespace_isbn_falls_back_to_title_author_year() {
        // Empty string and NULL both mean "no ISBN" (migration 057 rule): with
        // identical other fields the fallback key must be the same.
        let empty = book_dedup_key(
            Some("   "),
            "Le Petit Prince",
            Some("Saint-Exupéry"),
            Some(1943),
        );
        let none = book_dedup_key(None, "Le Petit Prince", Some("Saint-Exupéry"), Some(1943));
        assert!(empty.starts_with("ta:"));
        assert_eq!(
            empty, none,
            "empty-string and NULL ISBN must behave identically"
        );
    }

    #[test]
    fn fallback_distinguishes_different_books() {
        let a = book_dedup_key(None, "Dune", Some("Herbert"), Some(1965));
        let b = book_dedup_key(None, "Dune Messiah", Some("Herbert"), Some(1969));
        assert_ne!(a, b);
    }

    #[test]
    fn fallback_ignores_punctuation_whitespace_and_case() {
        // Diacritics are kept identical on both sides (they are not folded);
        // only punctuation, extra whitespace and case differ -> same key.
        let a = book_dedup_key(None, "L'Étranger", Some("Albert  Camus"), Some(1942));
        let b = book_dedup_key(None, "l'étranger!", Some("albert camus"), Some(1942));
        assert_eq!(a, b);
    }

    // --- contact ---

    #[test]
    fn contact_same_email_matches_case_insensitively() {
        let a = contact_dedup_key(Some("  Alice@Example.COM "), None, "Alice");
        let b = contact_dedup_key(Some("alice@example.com"), Some("0612345678"), "A. Liddell");
        assert_eq!(a, b, "email wins and is case/space-insensitive");
        assert_eq!(a.as_deref(), Some("email:alice@example.com"));
    }

    #[test]
    fn contact_phone_matches_across_formatting() {
        let a = contact_dedup_key(None, Some("+33 6 12 34 56 78"), "Bob");
        let b = contact_dedup_key(None, Some("+33612345678"), "Robert");
        assert_eq!(a, b);
        assert_eq!(a.as_deref(), Some("phone:33612345678"));
    }

    #[test]
    fn contact_falls_back_to_name_then_none() {
        let by_name = contact_dedup_key(None, None, "  Carol   Danvers ");
        assert_eq!(by_name.as_deref(), Some("name:carol danvers"));
        // No email, no phone, blank name -> no usable identity.
        assert_eq!(contact_dedup_key(None, Some("  "), "   "), None);
    }

    #[test]
    fn contact_phone_and_name_do_not_collide() {
        // A name normalizing to "123" must not equal a phone "123".
        let phone = contact_dedup_key(None, Some("123"), "ignored");
        let name = contact_dedup_key(None, None, "123");
        assert_ne!(phone, name);
    }

    // --- author / tag ---

    #[test]
    fn author_key_normalizes_and_rejects_empty() {
        assert_eq!(
            author_dedup_key("Ursula K.  Le Guin"),
            author_dedup_key("ursula k le guin!")
        );
        assert_eq!(author_dedup_key("   "), None);
    }

    #[test]
    fn tag_key_normalizes_and_rejects_empty() {
        // Same source string, differing only by case / punctuation / whitespace.
        // (A hyphen vs a space is a different source -> accepted miss, like diacritics.)
        assert_eq!(
            tag_dedup_key("Science Fiction"),
            tag_dedup_key("  science   fiction! ")
        );
        assert_ne!(tag_dedup_key("sci-fi"), tag_dedup_key("fantasy"));
        assert_eq!(tag_dedup_key(""), None);
    }
}
