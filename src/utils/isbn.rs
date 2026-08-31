//! ISBN-10 ↔ ISBN-13 conversion helpers.
//!
//! Cover sources index images under a specific ISBN form, so a lookup with only
//! the scanned form misses covers catalogued under the other form. These helpers
//! provide the alternate form so a cover sub-lookup can retry on a miss.
//!
//! All check-digit arithmetic is delegated to the `isbn2` crate (no hand-rolled
//! validation), matching the reuse in `librius/src/utils/isbn.rs`.

use isbn2::{Isbn, Isbn10, Isbn13};
use std::str::FromStr;

/// Strip hyphens and spaces and trim surrounding whitespace.
///
/// Public because every ISBN comparison in the codebase needs the same idea of
/// "the same ISBN written differently": a catalogue that stores
/// `978-1-61729-455-6` and a wishlist that stores `9781617294556` are the same
/// book, and an equality filter on the raw strings says they are not.
pub fn plain(isbn: &str) -> String {
    isbn.trim().replace(['-', ' '], "")
}

/// Convert an ISBN string to its ISBN-13 plain (no-hyphen) form.
///
/// Returns `None` if the input is neither a valid ISBN-10 nor a valid ISBN-13.
pub fn to_isbn13(isbn_input: &str) -> Option<String> {
    match Isbn::from_str(&plain(isbn_input)).ok()? {
        Isbn::_13(i) => Some(i.to_string()),
        Isbn::_10(i) => Some(Isbn13::from(i).to_string()),
    }
}

/// Convert an ISBN string to its ISBN-10 plain (no-hyphen) form.
///
/// Returns `None` if the input is invalid or has no ISBN-10 equivalent:
/// 979-prefixed ISBN-13 numbers cannot be represented as ISBN-10.
pub fn to_isbn10(isbn_input: &str) -> Option<String> {
    match Isbn::from_str(&plain(isbn_input)).ok()? {
        Isbn::_10(i) => Some(i.to_string()),
        Isbn::_13(i) => Isbn10::try_from(i).ok().map(|i| i.to_string()),
    }
}

/// The ISBN-13 form when the value parses, the input untouched otherwise.
///
/// The lossy fallback is deliberate: it makes this usable as a grouping key for
/// values that are not valid ISBNs at all, which a catalogue can perfectly well
/// contain, and those only ever match themselves.
///
/// The hub catalog cache (`api/frb/hub_catalog.rs`) follows the same convention
/// but calls [`to_isbn13`] directly on purpose: it needs to know whether the
/// value parsed, because an unparseable key must never be folded into another
/// row. Do not "simplify" those two sites to this function.
pub fn canonical(isbn_input: &str) -> String {
    to_isbn13(isbn_input).unwrap_or_else(|| isbn_input.to_string())
}

/// Every form an equality lookup should try for one ISBN: the value as given,
/// its punctuation-free form, and both lengths.
///
/// Single home for the expansion, so a lookup cannot quietly cover fewer forms
/// than its neighbour. `wishlist_service` used to expand to the raw value plus
/// the *other* length only, which missed a hyphenated wish against a clean
/// stored row: the clean same-length form was in neither set.
///
/// Deduplicated, and never empty: the input always comes back.
pub fn lookup_forms(isbn_input: &str) -> Vec<String> {
    let mut forms = vec![isbn_input.to_string()];
    for candidate in [
        plain(isbn_input),
        to_isbn13(isbn_input).unwrap_or_default(),
        to_isbn10(isbn_input).unwrap_or_default(),
    ] {
        if !candidate.is_empty() && !forms.contains(&candidate) {
            forms.push(candidate);
        }
    }
    forms
}

/// Return the *other* length form of the given ISBN (10 ↔ 13), plain (no hyphens).
///
/// Used by cover lookups: on a miss with the scanned form, retry with the
/// alternate form. Returns `None` when the input is invalid or the alternate form
/// does not exist (a 979-prefixed ISBN-13 has no ISBN-10 equivalent).
pub fn alternate_isbn(isbn_input: &str) -> Option<String> {
    match Isbn::from_str(&plain(isbn_input)).ok()? {
        Isbn::_10(i) => Some(Isbn13::from(i).to_string()),
        Isbn::_13(i) => Isbn10::try_from(i).ok().map(|i| i.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical pair from the Wikipedia ISBN article: 0-306-40615-2 / 978-0-306-40615-7.
    const ISBN10: &str = "0306406152";
    const ISBN13: &str = "9780306406157";

    #[test]
    fn plain_strips_punctuation_and_whitespace() {
        assert_eq!(plain(" 978-1-61729-455-6 "), "9781617294556");
        assert_eq!(plain("978 1 61729 455 6"), "9781617294556");
        assert_eq!(plain(ISBN13), ISBN13);
    }

    #[test]
    fn canonical_falls_back_to_the_input() {
        assert_eq!(canonical("978-0-306-40615-7"), ISBN13);
        assert_eq!(canonical(ISBN10), ISBN13);
        // Not an ISBN at all: usable as a grouping key, matching only itself.
        assert_eq!(canonical("not-an-isbn"), "not-an-isbn");
    }

    #[test]
    fn lookup_forms_cover_punctuation_and_both_lengths() {
        // The case the wishlist join missed: a hyphenated wish has to reach a
        // cleanly stored row, which needs the clean SAME-length form. The old
        // expansion offered the raw value and the other length only.
        let forms = lookup_forms("978-0-306-40615-7");
        assert!(forms.contains(&"978-0-306-40615-7".to_string()));
        assert!(forms.contains(&ISBN13.to_string()));
        assert!(forms.contains(&ISBN10.to_string()));
    }

    #[test]
    fn lookup_forms_never_lose_the_input() {
        assert_eq!(lookup_forms("garbage"), vec!["garbage".to_string()]);
        // No duplicates when every form collapses to the same string.
        assert_eq!(
            lookup_forms(ISBN13).iter().filter(|f| *f == ISBN13).count(),
            1
        );
    }

    #[test]
    fn to_isbn13_from_isbn10() {
        assert_eq!(to_isbn13(ISBN10).as_deref(), Some(ISBN13));
    }

    #[test]
    fn to_isbn13_from_isbn13_is_identity() {
        assert_eq!(to_isbn13(ISBN13).as_deref(), Some(ISBN13));
    }

    #[test]
    fn to_isbn13_accepts_hyphenated_input() {
        assert_eq!(to_isbn13("0-306-40615-2").as_deref(), Some(ISBN13));
    }

    #[test]
    fn to_isbn10_from_isbn13() {
        assert_eq!(to_isbn10(ISBN13).as_deref(), Some(ISBN10));
    }

    #[test]
    fn to_isbn10_from_isbn10_is_identity() {
        assert_eq!(to_isbn10(ISBN10).as_deref(), Some(ISBN10));
    }

    #[test]
    fn alternate_converts_both_directions() {
        assert_eq!(alternate_isbn(ISBN10).as_deref(), Some(ISBN13));
        assert_eq!(alternate_isbn(ISBN13).as_deref(), Some(ISBN10));
    }

    #[test]
    fn alternate_accepts_hyphenated_and_spaced_input() {
        assert_eq!(alternate_isbn("978-0-306-40615-7").as_deref(), Some(ISBN10));
        assert_eq!(alternate_isbn("  0 306 40615 2 ").as_deref(), Some(ISBN13));
    }

    #[test]
    fn invalid_input_returns_none() {
        assert_eq!(alternate_isbn("not-an-isbn"), None);
        assert_eq!(alternate_isbn("12345"), None);
        // Valid length but wrong check digit.
        assert_eq!(alternate_isbn("9780306406150"), None);
    }

    #[test]
    fn isbn13_with_979_prefix_has_no_isbn10() {
        // 9791090636071: valid ISBN-13 check digit, 979 prefix → no ISBN-10 form.
        const ISBN13_979: &str = "9791090636071";
        assert_eq!(to_isbn13(ISBN13_979).as_deref(), Some(ISBN13_979));
        assert_eq!(to_isbn10(ISBN13_979), None);
        assert_eq!(alternate_isbn(ISBN13_979), None);
    }
}
