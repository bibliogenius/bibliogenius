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

#[cfg(test)]
mod tests {
    use super::*;

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
