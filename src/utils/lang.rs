//! Language matching and target-language inference.
//!
//! Two concerns live here:
//! 1. **Matching** (`base_lang` / `lang_matches` / `lang_matches_any`): compare
//!    language tags tolerantly across 2-letter (ISO 639-1), 3-letter (ISO 639-2/3)
//!    and regional variants. Reused by federated search relevance scoring
//!    (`api/integrations.rs`) and by summary language coherence (`lookup_service`).
//! 2. **Inference** (`isbn_registration_group_lang` / `detect_text_lang` /
//!    `target_summary_language`): derive a single best-guess language for a book so
//!    an auto-filled summary can be rejected when it is in the wrong language
//!    (ADR-040 language-coherence addendum).

/// Strip regional/country suffix from a BCP 47 tag: "pt-BR" → "pt", "zh-TW" → "zh".
/// Already-simple codes like "fr" pass through unchanged.
pub fn base_lang(code: &str) -> &str {
    code.split(['-', '_']).next().unwrap_or(code)
}

/// Check if two language tags refer to the same language.
///
/// Handles 2-letter vs 3-letter codes and regional variants. An empty
/// `user_lang` matches anything (no preference expressed).
pub fn lang_matches(book_lang: &str, user_lang: &str) -> bool {
    if user_lang.is_empty() {
        return true;
    }
    // Strip regional codes before comparing: "pt-BR" → "pt"
    let b = base_lang(&book_lang.to_lowercase()).to_lowercase();
    let u = base_lang(&user_lang.to_lowercase()).to_lowercase();

    if b == u {
        return true;
    }

    // Simple mapping for common languages
    matches!(
        (b.as_str(), u.as_str()),
        ("en", "eng")
            | ("eng", "en")
            | ("fr", "fre")
            | ("fre", "fr")
            | ("fra", "fr")
            | ("fr", "fra")
            | ("de", "ger")
            | ("ger", "de")
            | ("deu", "de")
            | ("de", "deu")
            | ("es", "spa")
            | ("spa", "es")
            | ("it", "ita")
            | ("ita", "it")
            | ("pt", "por")
            | ("por", "pt")
            | ("nl", "dut")
            | ("dut", "nl")
            | ("nld", "nl")
            | ("nl", "nld")
            | ("ru", "rus")
            | ("rus", "ru")
            | ("ja", "jpn")
            | ("jpn", "ja")
            | ("zh", "chi")
            | ("chi", "zh")
            | ("zho", "zh")
            | ("zh", "zho")
            | ("ko", "kor")
            | ("kor", "ko")
            | ("ar", "ara")
            | ("ara", "ar")
    )
}

/// Check if a book language matches ANY of the user's preferred languages.
pub fn lang_matches_any(book_lang: &str, user_langs: &[String]) -> bool {
    if user_langs.is_empty() {
        return true;
    }
    user_langs.iter().any(|ul| lang_matches(book_lang, ul))
}

/// Map an ISBN's registration group to the language most likely used by its
/// publishers. Reliable, free, and offline — the workhorse signal for summary
/// language coherence (ADR-040). Returns `None` for an invalid ISBN or a group
/// with no clear single-language association.
///
/// Note: the registration group reflects the *publisher's country*, not strictly
/// the book's language (a French publisher may issue an English book), hence it is
/// only the first of several cascading signals.
pub fn isbn_registration_group_lang(isbn: &str) -> Option<&'static str> {
    let isbn13 = crate::utils::isbn::to_isbn13(isbn)?;

    // (EAN prefix + registration group, language). Longest prefix wins so the
    // 2-digit groups (978-84…) take precedence over any 1-digit overlap.
    const GROUPS: &[(&str, &str)] = &[
        // 979 prefixes
        ("97910", "fr"), // 979-10 France
        ("97911", "ko"), // 979-11 Korea
        ("97912", "it"), // 979-12 Italy
        ("9798", "en"),  // 979-8  United States
        // 978 one-digit groups
        ("9780", "en"), // 978-0 English
        ("9781", "en"), // 978-1 English
        ("9782", "fr"), // 978-2 French
        ("9783", "de"), // 978-3 German
        ("9784", "ja"), // 978-4 Japan
        ("9785", "ru"), // 978-5 former USSR / Russia
        ("9787", "zh"), // 978-7 China
        // 978 two-digit groups
        ("97884", "es"), // 978-84 Spain
        ("97885", "pt"), // 978-85 Brazil
        ("97888", "it"), // 978-88 Italy
        ("97889", "ko"), // 978-89 Korea
        ("97890", "nl"), // 978-90 Netherlands / Belgium (Dutch)
    ];

    GROUPS
        .iter()
        .filter(|(prefix, _)| isbn13.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, lang)| *lang)
}

/// Detect the dominant language of a text, returning an ISO 639-3 code (e.g.
/// "fra", "eng") only when the detection is *reliable*. Short or ambiguous text
/// yields `None` so callers never reject content on a weak guess.
///
/// Last-resort signal: source language tags are preferred when available.
pub fn detect_text_lang(text: &str) -> Option<String> {
    let info = whatlang::detect(text)?;
    if !info.is_reliable() {
        return None;
    }
    Some(info.lang().code().to_string())
}

/// Infer the single language a book's auto-filled summary should be in, via the
/// locked ADR-040 cascade (first available signal wins):
/// 1. ISBN registration group (reliable, offline)
/// 2. Title language via `whatlang` (noisy on short titles → below the ISBN group)
/// 3. The user's first preferred reading language (interface / device proxy)
///
/// Returns `None` when no signal is available; callers then apply no language
/// gate (there is no basis to reject a summary).
pub fn target_summary_language(isbn: &str, title: &str, user_langs: &[String]) -> Option<String> {
    if let Some(lang) = isbn_registration_group_lang(isbn) {
        return Some(lang.to_string());
    }
    if let Some(lang) = detect_text_lang(title) {
        return Some(lang);
    }
    user_langs.first().map(|s| base_lang(s).to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Matching ─────────────────────────────────────────────────────

    #[test]
    fn lang_matches_strips_regional_codes() {
        assert!(lang_matches("pt", "pt-BR"));
        assert!(lang_matches("pt-BR", "pt"));
        assert!(lang_matches("pt-BR", "pt-PT"));
        assert!(lang_matches("pt-PT", "pt-BR"));
    }

    #[test]
    fn lang_matches_chinese_variants() {
        assert!(lang_matches("zh", "zh-CN"));
        assert!(lang_matches("zh-CN", "zh"));
        assert!(lang_matches("zh-TW", "zh"));
        assert!(lang_matches("zh", "zh-TW"));
        assert!(lang_matches("zh-CN", "zh-TW"));
    }

    #[test]
    fn lang_matches_iso639_2_with_regional() {
        assert!(lang_matches("por", "pt-BR"));
        assert!(lang_matches("chi", "zh-TW"));
        assert!(lang_matches("zho", "zh-CN"));
    }

    #[test]
    fn lang_matches_simple_codes_still_work() {
        assert!(lang_matches("en", "en"));
        assert!(lang_matches("en", "eng"));
        assert!(lang_matches("fr", "fra"));
        assert!(lang_matches("pt", "por"));
    }

    #[test]
    fn lang_matches_empty_user_lang_matches_all() {
        assert!(lang_matches("anything", ""));
    }

    #[test]
    fn lang_matches_different_languages_dont_match() {
        assert!(!lang_matches("fr", "en"));
        assert!(!lang_matches("pt", "es"));
        assert!(!lang_matches("pt-BR", "es"));
    }

    #[test]
    fn lang_matches_any_with_regional_codes() {
        let user_langs = vec!["pt-BR".to_string(), "en".to_string()];
        assert!(lang_matches_any("pt", &user_langs));
        assert!(lang_matches_any("pt-PT", &user_langs));
        assert!(lang_matches_any("eng", &user_langs));
        assert!(!lang_matches_any("fr", &user_langs));
    }

    // ── ISBN registration group ──────────────────────────────────────

    #[test]
    fn isbn_group_maps_common_languages() {
        // 978-0 / 978-1 → English
        assert_eq!(isbn_registration_group_lang("9780306406157"), Some("en"));
        // 978-2 → French
        assert_eq!(isbn_registration_group_lang("9782070413119"), Some("fr"));
        // 978-3 → German
        assert_eq!(isbn_registration_group_lang("9783161484100"), Some("de"));
        // 978-84 → Spanish (2-digit group beats any 1-digit overlap)
        assert_eq!(isbn_registration_group_lang("9788437604947"), Some("es"));
        // 978-88 → Italian
        assert_eq!(isbn_registration_group_lang("9788806219338"), Some("it"));
        // 979-10 → French
        assert_eq!(isbn_registration_group_lang("9791090636071"), Some("fr"));
    }

    #[test]
    fn isbn_group_accepts_isbn10_and_hyphens() {
        assert_eq!(isbn_registration_group_lang("0306406152"), Some("en"));
        assert_eq!(
            isbn_registration_group_lang("978-2-07-041311-9"),
            Some("fr")
        );
    }

    #[test]
    fn isbn_group_returns_none_for_invalid() {
        assert_eq!(isbn_registration_group_lang("not-an-isbn"), None);
        assert_eq!(isbn_registration_group_lang(""), None);
    }

    // ── Text detection ───────────────────────────────────────────────

    #[test]
    fn detect_recognizes_clear_prose() {
        let fr = "Ceci est un roman français qui raconte l'histoire d'une famille \
                  de mineurs dans le nord de la France au dix-neuvième siècle.";
        let en = "This is an English novel telling the story of a mining family \
                  in the north of France during the nineteenth century.";
        assert!(lang_matches(&detect_text_lang(fr).unwrap(), "fr"));
        assert!(lang_matches(&detect_text_lang(en).unwrap(), "en"));
    }

    // ── Target-language cascade ──────────────────────────────────────

    #[test]
    fn target_isbn_group_wins_over_title_and_user() {
        // French ISBN, English title, English reading language → ISBN group wins.
        let target =
            target_summary_language("9782070413119", "Some Title", &["en".to_string()]).unwrap();
        assert!(lang_matches(&target, "fr"));
    }

    #[test]
    fn target_falls_back_to_title_detection() {
        // No usable ISBN → detect from a clearly English title-as-text.
        let target = target_summary_language(
            "",
            "This is clearly an English sentence about books and libraries.",
            &["fr".to_string()],
        )
        .unwrap();
        assert!(lang_matches(&target, "en"));
    }

    #[test]
    fn target_falls_back_to_user_language() {
        // No ISBN, untypable title → first reading language.
        let target = target_summary_language("", "x", &["de".to_string(), "en".to_string()]);
        assert_eq!(target.as_deref(), Some("de"));
    }

    #[test]
    fn target_none_when_no_signal() {
        assert_eq!(target_summary_language("", "", &[]), None);
    }
}
