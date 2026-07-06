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
