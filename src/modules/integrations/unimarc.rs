//! Shared UNIMARC parsing helpers used by SUDOC and BNF SRU integrations.

/// Pair of (firstname, surname) extracted from a UNIMARC author access field
/// (`700`, `701` or `702`). Either side may be missing.
pub type AuthorParts = (Option<String>, Option<String>);

/// Compose a single author label from UNIMARC access fields, falling back
/// to the `200 $f` statement of responsibility only as a last resort.
///
/// UNIMARC `200 $f` is free text by definition (e.g. "transcrit et présenté
/// par X") and must not be preferred over structured `7XX` access fields,
/// otherwise the author column gets filled with a sentence rather than a name.
///
/// Priority: `700` (main author) → `701` (alternative) → `702` (secondary,
/// e.g. translator/editor) → `200 $f`.
pub fn compose_author(
    primary: AuthorParts,
    secondary: AuthorParts,
    tertiary: AuthorParts,
    responsibility: Option<String>,
) -> Option<String> {
    join_name(primary)
        .or_else(|| join_name(secondary))
        .or_else(|| join_name(tertiary))
        .or(responsibility)
}

fn join_name(parts: AuthorParts) -> Option<String> {
    match parts {
        (Some(firstname), Some(surname)) => Some(format!("{} {}", firstname, surname)),
        (None, Some(surname)) => Some(surname),
        (Some(firstname), None) => Some(firstname),
        (None, None) => None,
    }
}

/// Extract a page count from a UNIMARC `215 $a` extent statement.
///
/// The extent is free text written by cataloguers: "438 p.", "1 vol. (456 p.)",
/// "1 volume (456 pages)", "XII-456 p.". The page count is the number that
/// directly precedes a "p" / "pages" token; every other number (volume count,
/// roman-numeral front matter, dimensions) is ignored. Returns `None` when no
/// such pair exists ("non paginé", "pagination multiple").
pub fn page_count_from_extent(extent: &str) -> Option<u32> {
    let mut last_number: Option<u32> = None;
    let separators =
        |c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | ':' | '-' | '[' | ']');
    for raw in extent.split(separators) {
        let token = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if token.is_empty() {
            continue;
        }
        let digits_len = token.chars().take_while(|c| c.is_ascii_digit()).count();
        let (digits, rest) = token.split_at(digits_len);
        let is_pages_word = rest.to_ascii_lowercase().starts_with('p');
        if digits.is_empty() {
            if is_pages_word && let Some(n) = last_number {
                return Some(n);
            }
            last_number = None;
            continue;
        }
        let number = digits.parse::<u32>().ok().filter(|n| *n > 0);
        if rest.is_empty() {
            last_number = number;
        } else if is_pages_word && number.is_some() {
            // "456p." with the unit glued to the number.
            return number;
        } else {
            last_number = None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_count_reads_the_number_before_the_pages_word() {
        assert_eq!(page_count_from_extent("438 p."), Some(438));
        assert_eq!(page_count_from_extent("1 volume (456 pages)"), Some(456));
        assert_eq!(page_count_from_extent("1 vol. (456 p.)"), Some(456));
        assert_eq!(page_count_from_extent("456p."), Some(456));
    }

    #[test]
    fn page_count_skips_front_matter_and_volume_counts() {
        assert_eq!(page_count_from_extent("XII-456 p."), Some(456));
        assert_eq!(
            page_count_from_extent("1 vol. (XV-456 p.) ; 24 cm"),
            Some(456)
        );
        assert_eq!(page_count_from_extent("2 vol. (1200 p.)"), Some(1200));
    }

    #[test]
    fn page_count_is_none_without_a_pages_number() {
        assert_eq!(page_count_from_extent("non paginé"), None);
        assert_eq!(page_count_from_extent("1 vol. (pagination multiple)"), None);
        assert_eq!(page_count_from_extent("1 vol. ; 24 cm"), None);
        assert_eq!(page_count_from_extent(""), None);
        assert_eq!(page_count_from_extent("0 p."), None);
    }

    #[test]
    fn prefers_700_over_other_fields() {
        let author = compose_author(
            (Some("Antonio".into()), Some("Pigafetta".into())),
            (Some("Xavier de".into()), Some("Castro".into())),
            (Some("Xavier de".into()), Some("Castro".into())),
            Some("transcrite, présentée & annotée par Xavier de Castro".into()),
        );
        assert_eq!(author.as_deref(), Some("Antonio Pigafetta"));
    }

    #[test]
    fn falls_back_to_701_when_700_missing() {
        let author = compose_author(
            (None, None),
            (Some("Jane".into()), Some("Doe".into())),
            (None, None),
            Some("free text".into()),
        );
        assert_eq!(author.as_deref(), Some("Jane Doe"));
    }

    #[test]
    fn falls_back_to_702_when_700_and_701_missing() {
        let author = compose_author(
            (None, None),
            (None, None),
            (Some("Xavier de".into()), Some("Castro".into())),
            Some("free text".into()),
        );
        assert_eq!(author.as_deref(), Some("Xavier de Castro"));
    }

    #[test]
    fn uses_200f_only_as_last_resort() {
        let author = compose_author(
            (None, None),
            (None, None),
            (None, None),
            Some("anonymous".into()),
        );
        assert_eq!(author.as_deref(), Some("anonymous"));
    }

    #[test]
    fn returns_none_when_nothing_found() {
        let author = compose_author((None, None), (None, None), (None, None), None);
        assert!(author.is_none());
    }

    #[test]
    fn handles_surname_only() {
        let author = compose_author(
            (None, Some("Pigafetta".into())),
            (None, None),
            (None, None),
            None,
        );
        assert_eq!(author.as_deref(), Some("Pigafetta"));
    }
}
