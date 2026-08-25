//! Publication year extraction from the free-form dates metadata sources return.
//!
//! No two external catalogues agree on the shape of a publication date:
//! OpenLibrary returns free text (`"Jan 01, 2004"`, `"c1998"`), Google Books
//! returns an ISO date (`"2004-01-01"`) or a bare year, BNF and SUDOC return an
//! integer. Consumers used to guess: reading the first four characters mangles
//! the free-text ones into `"Jan "`, and parsing the whole string as an integer
//! silently drops the ISO ones. Both failures reached the user as a wrong or
//! missing year on the add/scan form.
//!
//! Every source normalises through this module, so `publication_year` carries a
//! canonical four-digit year by the time it leaves the lookup.

/// Bounds a plausible publication year, so a volume number or a page count
/// caught in the same string cannot pass for one.
const MIN_YEAR: i32 = 1000;
const MAX_YEAR: i32 = 2999;

/// Extract the publication year from a source-provided date string.
///
/// Returns the first standalone four-digit group in a plausible range, so
/// `"Jan 01, 2004"`, `"2004-01-01"`, `"c2004"` and `"2004"` all yield `2004`.
/// Digit groups of any other length are skipped rather than truncated: a run of
/// five digits is not a year, and truncating it would invent one.
pub fn parse_year(raw: &str) -> Option<i32> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i - start == 4
            && let Ok(year) = raw[start..i].parse::<i32>()
            && (MIN_YEAR..=MAX_YEAR).contains(&year)
        {
            return Some(year);
        }
    }
    None
}

/// Canonical `"YYYY"` rendering of a source-provided date string, or `None`
/// when it carries no plausible year.
pub fn normalize_year(raw: &str) -> Option<String> {
    parse_year(raw).map(|y| y.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_year() {
        assert_eq!(parse_year("1998"), Some(1998));
    }

    #[test]
    fn parses_iso_dates() {
        assert_eq!(parse_year("1998-03-01"), Some(1998));
        assert_eq!(parse_year("2010-01"), Some(2010));
    }

    /// The reported bug: OpenLibrary's free-text `publish_date` used to reach
    /// the year field as its first four characters, i.e. a month prefix.
    #[test]
    fn parses_a_month_first_free_text_date() {
        assert_eq!(parse_year("Jan 01, 2004"), Some(2004));
        assert_eq!(parse_year("March 15, 2001"), Some(2001));
        assert_eq!(parse_year("1 janvier 2004"), Some(2004));
    }

    #[test]
    fn parses_a_year_glued_to_a_cataloguing_prefix() {
        assert_eq!(parse_year("c1998"), Some(1998));
        assert_eq!(parse_year("DL 2004"), Some(2004));
        assert_eq!(parse_year("publié en 2001."), Some(2001));
    }

    #[test]
    fn rejects_strings_without_a_year() {
        assert_eq!(parse_year("n/a"), None);
        assert_eq!(parse_year(""), None);
        assert_eq!(parse_year("Jan"), None);
    }

    #[test]
    fn rejects_digit_groups_that_are_not_years() {
        // Not truncated to 1234 / 2004: neither run is a four-digit group.
        assert_eq!(parse_year("12345"), None);
        assert_eq!(parse_year("20045"), None);
        // Out of the plausible range.
        assert_eq!(parse_year("-0500-01-01"), None);
        assert_eq!(parse_year("0042"), None);
    }

    #[test]
    fn normalizes_to_a_four_digit_string() {
        assert_eq!(normalize_year("Jan 01, 2004").as_deref(), Some("2004"));
        assert_eq!(normalize_year("n/a"), None);
    }
}
