pub mod bnf;
pub mod google_books;
pub mod inventaire;
pub mod openlibrary;
pub mod sudoc;
pub mod unimarc;

/// Identifying User-Agent sent on outbound requests to external bibliographic
/// APIs. A non-empty UA is REQUIRED by OpenLibrary — it returns 403 on its
/// `/api/books`, `/isbn`, `/search.json` endpoints without one — and is good
/// etiquette for Inventaire/Wikidata. A URL (not an email) is used as the contact
/// channel: it has no mailbox to maintain and leaks no personal address.
pub const API_USER_AGENT: &str = "BiblioGenius/1.0 (+https://bibliogenius.org)";

/// Decoded, entity-unescaped content of an XML text event, or `""` when the
/// payload is malformed (quick-xml 0.41 split the old `BytesText::unescape`
/// into `decode()` + the free `escape::unescape()`).
pub(crate) fn xml_text_content(e: &quick_xml::events::BytesText) -> String {
    e.decode()
        .ok()
        .and_then(|t| quick_xml::escape::unescape(&t).ok().map(|u| u.into_owned()))
        .unwrap_or_default()
}

/// The text an entity reference stands for.
///
/// quick-xml reports `&amp;`, `&apos;` and `&#233;` as their own events
/// rather than folding them into the surrounding text, so a parser that
/// reads text alone drops them AND splits the value around them. Numeric
/// references resolve to a char, named ones to their predefined string;
/// anything else (a DTD-declared entity, which these catalogues do not use)
/// resolves to nothing rather than to a guess.
pub(crate) fn xml_entity_text(e: &quick_xml::events::BytesRef) -> Option<String> {
    if let Ok(Some(c)) = e.resolve_char_ref() {
        return Some(c.to_string());
    }
    let name = String::from_utf8_lossy(e).to_string();
    quick_xml::escape::resolve_predefined_entity(&name).map(|s| s.to_string())
}
