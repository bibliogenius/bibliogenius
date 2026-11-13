//! Text helpers shared by the probe and the lookup: comparing titles across
//! accents and punctuation, building the slug a site puts in its book URLs,
//! and finding ISBNs in a page.

use unicode_normalization::UnicodeNormalization;

/// Short words left out when comparing titles: they match everywhere.
const STOP_WORDS: [&str; 21] = [
    "les", "des", "une", "dans", "pour", "par", "sur", "avec", "aux", "the", "and", "tome",
    "livre", "edition", "est", "qui", "que", "son", "ses", "mon", "nos",
];

/// Lowercase ASCII words separated by single spaces: accents dropped, every
/// other character a separator. `"L'Île des Âmes"` gives `"l ile des ames"`.
pub fn fold(s: &str) -> String {
    let ascii: String = s.nfkd().filter(char::is_ascii).collect();
    ascii
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The words of a title worth comparing: three letters or more, no stop word.
pub fn title_words(title: &str) -> Vec<String> {
    fold(title)
        .split(' ')
        .filter(|w| w.len() >= 3 && !STOP_WORDS.contains(w))
        .map(str::to_owned)
        .collect()
}

/// How a site turns an apostrophe into a slug: `j'ai` gives `jai` or `j-ai`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Apostrophe {
    Drop,
    Dash,
}

/// The slug a site derives from a title: lowercase ASCII words joined by `-`.
pub fn slugify(s: &str, apostrophe: Apostrophe) -> String {
    let replacement = match apostrophe {
        Apostrophe::Drop => "",
        Apostrophe::Dash => "-",
    };
    let s = s
        .replace('\u{2019}', "'")
        .replace('œ', "oe")
        .replace('Œ', "Oe")
        .replace('\'', replacement);
    let ascii: String = s.nfkd().filter(char::is_ascii).collect();
    ascii
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// The orders a site may put a title in: as given, then, for
/// `"Series, tome N : Title"`, the title before the series, the title alone
/// and the series alone.
pub fn title_variants(title: &str) -> Vec<String> {
    let mut out = vec![title.to_owned()];
    if let Some((series, own)) = title.rsplit_once(':') {
        let (series, own) = (series.trim(), own.trim());
        out.push(format!("{own} {series}"));
        out.push(own.to_owned());
        out.push(series.to_owned());
    }
    out.retain(|t| !t.is_empty());
    out.dedup();
    out
}

/// The first author's name as given, then with the first or the last word
/// moved to the other end (`"Victor Hugo"` and `"Hugo Victor"`).
pub fn author_variants(authors: &str) -> Vec<String> {
    let first = authors.split(',').next().unwrap_or("").trim();
    let parts: Vec<&str> = first.split_whitespace().collect();
    let mut out = vec![first.to_owned()];
    if parts.len() >= 2 {
        out.push([&parts[1..], &parts[..1]].concat().join(" "));
        out.push(
            [&parts[parts.len() - 1..], &parts[..parts.len() - 1]]
                .concat()
                .join(" "),
        );
    }
    let mut unique: Vec<String> = Vec::new();
    for a in out {
        if !a.is_empty() && !unique.contains(&a) {
            unique.push(a);
        }
    }
    unique
}

/// Every 13-digit ISBN (978 or 979) in `s`, in order. Digits are read in
/// runs: a longer run yields the ISBNs found by scanning it left to right.
pub fn find_isbns(s: &str) -> Vec<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 13 <= bytes.len() {
        let window = &bytes[i..i + 13];
        if (window.starts_with(b"978") || window.starts_with(b"979"))
            && window.iter().all(u8::is_ascii_digit)
        {
            out.push(String::from_utf8_lossy(window).into_owned());
            i += 13;
        } else {
            i += 1;
        }
    }
    out
}

/// `s` without the spaces and hyphens that ISBNs are printed with, so
/// `978-2-07-012345-6` is found as `9782070123456`.
pub fn strip_isbn_separators(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '\u{2010}' | '\u{2011}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_drops_accents_and_punctuation() {
        assert_eq!(
            fold("L'Île des Âmes - Édition Collector"),
            "l ile des ames edition collector"
        );
        assert_eq!(fold("  "), "");
    }

    #[test]
    fn title_words_skip_short_and_stop_words() {
        assert_eq!(
            title_words("Le Palais des miroirs, tome 2"),
            ["palais", "miroirs"]
        );
    }

    #[test]
    fn slugify_follows_the_apostrophe_rule() {
        assert_eq!(
            slugify("J’ai lu l'œuvre", Apostrophe::Drop),
            "jai-lu-loeuvre"
        );
        assert_eq!(
            slugify("J’ai lu l'œuvre", Apostrophe::Dash),
            "j-ai-lu-l-oeuvre"
        );
        assert_eq!(
            slugify("N'y échappe pas !", Apostrophe::Drop),
            "ny-echappe-pas"
        );
    }

    #[test]
    fn title_variants_reorder_series_and_title() {
        assert_eq!(
            title_variants("La Passe-miroir, tome 1 : Les Fiancés de l'hiver"),
            [
                "La Passe-miroir, tome 1 : Les Fiancés de l'hiver",
                "Les Fiancés de l'hiver La Passe-miroir, tome 1",
                "Les Fiancés de l'hiver",
                "La Passe-miroir, tome 1",
            ]
        );
        assert_eq!(title_variants("Martin Eden"), ["Martin Eden"]);
    }

    #[test]
    fn author_variants_try_both_name_orders() {
        assert_eq!(
            author_variants("Victor Hugo, Paul Meurice"),
            ["Victor Hugo", "Hugo Victor"]
        );
        assert_eq!(
            author_variants("Jean-Marie Gustave Le Clézio"),
            [
                "Jean-Marie Gustave Le Clézio",
                "Gustave Le Clézio Jean-Marie",
                "Clézio Jean-Marie Gustave Le"
            ]
        );
        assert_eq!(author_variants("Homère"), ["Homère"]);
        assert!(author_variants("").is_empty());
    }

    #[test]
    fn find_isbns_reads_13_digit_runs() {
        assert_eq!(
            find_isbns("/livre/ma-reine-9782378802967/ and 9791037509213 but not 1234567890123"),
            ["9782378802967", "9791037509213"]
        );
        assert!(find_isbns("978237880296").is_empty());
    }

    #[test]
    fn isbn_separators_are_stripped() {
        assert_eq!(
            strip_isbn_separators("ISBN : 978-2-07\u{2011}012345 6"),
            "ISBN:9782070123456"
        );
    }
}
