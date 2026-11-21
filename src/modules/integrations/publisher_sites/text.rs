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
    // Ligatures have no decomposition: written out, as sites write them.
    let s = s
        .replace('œ', "oe")
        .replace('Œ', "Oe")
        .replace('æ', "ae")
        .replace('Æ', "Ae");
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
/// and the series alone. A title without a colon that a spaced dash cuts
/// (`"Title - Subtitle"`) is read the same way: which part is the book's own
/// title cannot be told from the text, so both are tried alone.
pub fn title_variants(title: &str) -> Vec<String> {
    let mut out = vec![title.to_owned()];
    let parts = title
        .rsplit_once(':')
        .map(|(series, own)| (series.trim(), own.trim()))
        .or_else(|| split_at_spaced_dash(title));
    if let Some((series, own)) = parts {
        out.push(format!("{own} {series}"));
        out.push(own.to_owned());
        out.push(series.to_owned());
    }
    out.retain(|t| !t.is_empty());
    out.dedup();
    out
}

/// A title cut at its first dash standing between two spaces (hyphen, en or
/// em dash): what catalogues put between a title and its subtitle. A hyphen
/// inside a word (`"A-t-on"`) cuts nothing.
fn split_at_spaced_dash(title: &str) -> Option<(&str, &str)> {
    let (at, dash) = title.char_indices().find(|&(i, c)| {
        matches!(c, '-' | '\u{2013}' | '\u{2014}')
            && title[..i].ends_with(char::is_whitespace)
            && title[i + c.len_utf8()..].starts_with(char::is_whitespace)
    })?;
    let (main, subtitle) = (title[..at].trim(), title[at + dash.len_utf8()..].trim());
    (!main.is_empty() && !subtitle.is_empty()).then_some((main, subtitle))
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

/// Registrant ranges of the French-language ISBN groups, from the ISBN
/// agency's range table: (first seven digits after the group, registrant
/// length). A range runs up to the next one.
const RANGES_978_2: &[(u32, usize)] = &[
    (0, 2),
    (2_000_000, 3),
    (3_500_000, 5),
    (4_000_000, 3),
    (4_870_000, 6),
    (4_950_000, 3),
    (4_960_000, 4),
    (4_967_000, 5),
    (4_970_000, 3),
    (5_280_000, 4),
    (5_300_000, 3),
    (7_000_000, 4),
    (8_400_000, 5),
    (9_000_000, 6),
    (9_198_000, 5),
    (9_198_100, 6),
    (9_199_430, 7),
    (9_199_690, 6),
    (9_500_000, 7),
];
const RANGES_979_10: &[(u32, usize)] = &[
    (0, 2),
    (2_000_000, 3),
    (7_000_000, 4),
    (9_000_000, 5),
    (9_760_000, 6),
];

/// Where an ISBN-13 of a French-language group splits: the end of its
/// group and of its publisher prefix, or `None` for another group (their
/// ranges are not known here) or for anything but thirteen digits.
fn isbn_parts(isbn: &str) -> Option<(usize, usize)> {
    let (group, ranges) = if isbn.starts_with("9782") {
        (4, RANGES_978_2)
    } else if isbn.starts_with("97910") {
        (5, RANGES_979_10)
    } else {
        return None;
    };
    if isbn.len() != 13 || !isbn.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest: u32 = isbn[group..group + 7].parse().ok()?;
    let (_, len) = ranges.iter().rev().find(|(start, _)| *start <= rest)?;
    Some((group, group + len))
}

/// The publisher prefix of an ISBN-13 (`9782070360024` -> `978207`), or
/// `None` outside the French-language groups.
pub fn publisher_prefix(isbn: &str) -> Option<String> {
    let (_, publisher) = isbn_parts(isbn)?;
    Some(isbn[..publisher].to_owned())
}

/// An ISBN-13 with its hyphens (`9782818055113` -> `978-2-8180-5511-3`), or
/// `None` outside the French-language groups.
pub fn hyphenated_isbn(isbn: &str) -> Option<String> {
    let (group, publisher) = isbn_parts(isbn)?;
    Some(format!(
        "{}-{}-{}-{}-{}",
        &isbn[..3],
        &isbn[3..group],
        &isbn[group..publisher],
        &isbn[publisher..12],
        &isbn[12..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_writes_ligatures_out() {
        assert_eq!(
            fold("Le Cœur de l'Œuvre, Æsope"),
            "le coeur de l oeuvre aesope"
        );
    }

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
    fn title_variants_cut_a_title_at_a_spaced_dash() {
        assert_eq!(
            title_variants("Donner à manger - Politique d'un geste ordinaire"),
            [
                "Donner à manger - Politique d'un geste ordinaire",
                "Politique d'un geste ordinaire Donner à manger",
                "Politique d'un geste ordinaire",
                "Donner à manger",
            ]
        );
        // The first dash cuts, whichever dash the catalogue used.
        assert_eq!(
            title_variants("Le Grand Cahier \u{2013} La Preuve \u{2013} Le Troisième Mensonge")[3],
            "Le Grand Cahier"
        );
        // A hyphen inside a word is no separator.
        assert_eq!(title_variants("A-t-on besoin d'un chef ?").len(), 1);
        // A colon is read first, the dash staying in its part.
        assert_eq!(
            title_variants("Saga : L'Aube - Le Soir")[2],
            "L'Aube - Le Soir"
        );
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

    #[test]
    fn reads_a_publisher_prefix_from_an_isbn() {
        assert_eq!(publisher_prefix("9782070360024").as_deref(), Some("978207"));
        assert_eq!(
            publisher_prefix("9782864329404").as_deref(),
            Some("978286432")
        );
        assert_eq!(
            publisher_prefix("9782919421589").as_deref(),
            Some("9782919421")
        );
        assert_eq!(
            publisher_prefix("9791032900123").as_deref(),
            Some("97910329")
        );
        assert_eq!(
            publisher_prefix("9791097160012").as_deref(),
            Some("9791097160")
        );
        // Another language group, or not an ISBN-13.
        assert_eq!(publisher_prefix("9780141439518"), None);
        assert_eq!(publisher_prefix("978207036002X"), None);
        assert_eq!(publisher_prefix("978207"), None);
        assert_eq!(
            hyphenated_isbn("9782818055113").as_deref(),
            Some("978-2-8180-5511-3")
        );
        assert_eq!(
            hyphenated_isbn("9791032900123").as_deref(),
            Some("979-10-329-0012-3")
        );
        assert_eq!(hyphenated_isbn("9780141439518"), None);
    }
}
