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
fn clean(isbn: &str) -> String {
    isbn.trim().replace(['-', ' '], "")
}

/// Convert an ISBN string to its ISBN-13 plain (no-hyphen) form.
///
/// Returns `None` if the input is neither a valid ISBN-10 nor a valid ISBN-13.
pub fn to_isbn13(isbn_input: &str) -> Option<String> {
    match Isbn::from_str(&clean(isbn_input)).ok()? {
        Isbn::_13(i) => Some(i.to_string()),
        Isbn::_10(i) => Some(Isbn13::from(i).to_string()),
    }
}

/// Convert an ISBN string to its ISBN-10 plain (no-hyphen) form.
///
/// Returns `None` if the input is invalid or has no ISBN-10 equivalent:
/// 979-prefixed ISBN-13 numbers cannot be represented as ISBN-10.
pub fn to_isbn10(isbn_input: &str) -> Option<String> {
    match Isbn::from_str(&clean(isbn_input)).ok()? {
        Isbn::_10(i) => Some(i.to_string()),
        Isbn::_13(i) => Isbn10::try_from(i).ok().map(|i| i.to_string()),
    }
}

/// Return the *other* length form of the given ISBN (10 ↔ 13), plain (no hyphens).
///
/// Used by cover lookups: on a miss with the scanned form, retry with the
/// alternate form. Returns `None` when the input is invalid or the alternate form
/// does not exist (a 979-prefixed ISBN-13 has no ISBN-10 equivalent).
pub fn alternate_isbn(isbn_input: &str) -> Option<String> {
    match Isbn::from_str(&clean(isbn_input)).ok()? {
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
