use crate::inventaire_client::AuthorMetadata;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::API_USER_AGENT;

#[derive(Debug, Serialize, Deserialize)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<AuthorMetadata>,
    pub publisher: Option<String>,
    /// Canonical four-digit year (e.g. `"2004"`), never a full date. Sources
    /// that answer a free-form date reduce it through [`crate::utils::year`];
    /// those that answer an integer are canonical by construction.
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
    pub summary: Option<String>,
    pub page_count: Option<u32>,
}

const OPENLIBRARY_BASE_URL: &str = "https://openlibrary.org";

/// Envelope of the Read API (`/api/volumes/brief/isbn/{isbn}.json`).
///
/// A known ISBN answers `{ "records": { "/books/OL…M": { "isbns": […],
/// "data": {…} } }, "items": […] }`; an unknown one answers a bare `[]` with
/// HTTP 200. The `data` object is the same record the legacy Books API
/// (`/api/books?bibkeys=ISBN:…&jscmd=data`) used to serve. That legacy route
/// answers 404 for every key since 2026-09 (openlibrary issue #13669) and its
/// documentation already flagged it as "may be phased out", hence the move.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ReadApiResponse {
    Found {
        records: HashMap<String, ReadApiRecord>,
    },
    /// The bare `[]`: OpenLibrary does not know this ISBN. The elements are
    /// never read; the variant only has to deserialize from a JSON array.
    Absent(#[allow(dead_code)] Vec<serde_json::Value>),
}

#[derive(Debug, Deserialize)]
struct ReadApiRecord {
    /// Every ISBN the matched edition carries, in whichever forms OpenLibrary
    /// stores them (an ISBN-10 query is answered with an edition that may list
    /// only its ISBN-13).
    #[serde(default)]
    isbns: Vec<String>,
    data: OpenLibraryBook,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryBook {
    title: String,
    authors: Option<Vec<OpenLibraryAuthor>>,
    publishers: Option<Vec<OpenLibraryPublisher>>,
    publish_date: Option<String>,
    cover: Option<OpenLibraryCover>,
    number_of_pages: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryAuthor {
    name: String,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryPublisher {
    name: String,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryCover {
    medium: Option<String>,
    large: Option<String>,
}

/// Resolve an ISBN through OpenLibrary. `Err` covers both "OpenLibrary did not
/// answer" and "OpenLibrary does not know this ISBN" (`"Book not found"`), the
/// contract every caller in the lookup chain already relies on.
pub async fn fetch_book_metadata(isbn: &str) -> Result<BookMetadata, String> {
    let Some(book) = fetch_book_record_at(OPENLIBRARY_BASE_URL, isbn).await? else {
        return Err("Book not found".to_string());
    };
    // Fetch description from edition/work API
    let summary = fetch_description(isbn).await;
    Ok(metadata_from_book(&book, summary))
}

/// Fetch the edition record for `isbn` from the Read API, with an injectable
/// endpoint so the found / absent / outage branches run against a mock server.
///
/// `Ok(None)` is OpenLibrary's own answer that the ISBN is unknown; `Err` is a
/// transport, HTTP or parse failure.
async fn fetch_book_record_at(
    base_url: &str,
    isbn: &str,
) -> Result<Option<OpenLibraryBook>, String> {
    // Encoded for the same reason as the cover lookup: the ISBN column has no
    // validator, so a hand-typed "/" or "?" would otherwise alter the path.
    let url = format!(
        "{}/api/volumes/brief/isbn/{}.json",
        base_url,
        urlencoding::encode(isbn)
    );

    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to send request: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!(
            "Open Library API returned status: {}",
            resp.status()
        ));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    let parsed: ReadApiResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse JSON: {}", e))?;

    match parsed {
        ReadApiResponse::Found { records } => Ok(select_record(records, isbn)),
        ReadApiResponse::Absent(_) => Ok(None),
    }
}

/// Pick the record to trust for `isbn` among those the Read API returned.
///
/// The API only returns records for the queried key, so this is a guard, not a
/// search: a record that lists ISBNs must list one form of the queried ISBN
/// (see [`crate::utils::isbn::lookup_forms`]), otherwise it is discarded rather
/// than filed under a scan it does not belong to. A record without any ISBN
/// cannot be checked and is kept. Records are visited in key order so the
/// same ISBN always resolves to the same edition.
fn select_record(records: HashMap<String, ReadApiRecord>, isbn: &str) -> Option<OpenLibraryBook> {
    let wanted = crate::utils::isbn::lookup_forms(isbn);
    let mut records: Vec<(String, ReadApiRecord)> = records.into_iter().collect();
    records.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, record) in records {
        let listed = record
            .isbns
            .iter()
            .map(|i| crate::utils::isbn::plain(i))
            .collect::<Vec<_>>();
        if listed.is_empty() || listed.iter().any(|i| wanted.contains(i)) {
            return Some(record.data);
        }
        tracing::debug!(
            "OpenLibrary record {} lists {:?}, none of which is {}: discarded",
            key,
            listed,
            isbn
        );
    }
    None
}

/// Project an OpenLibrary record onto the shared metadata shape.
///
/// Split from the fetch so the field mapping stays testable without a network
/// call: this is where `publish_date` — the one free-text field of the record —
/// is reduced, and where a mis-wired field would go unnoticed.
fn metadata_from_book(book: &OpenLibraryBook, summary: Option<String>) -> BookMetadata {
    let authors = book
        .authors
        .as_ref()
        .map(|a| {
            a.iter()
                .filter(|auth| {
                    let n = auth.name.trim();
                    !n.eq_ignore_ascii_case("unknown author") && !n.eq_ignore_ascii_case("unknown")
                })
                .map(|auth| AuthorMetadata {
                    name: auth.name.clone(),
                    birth_year: None,
                    death_year: None,
                    image_url: None,
                    bio: None,
                })
                .collect()
        })
        .unwrap_or_default();

    let publisher = book
        .publishers
        .as_ref()
        .and_then(|p| p.first().map(|publ| publ.name.clone()));

    let cover_url = book
        .cover
        .as_ref()
        .and_then(|c| c.large.clone().or(c.medium.clone()));

    BookMetadata {
        title: book.title.clone(),
        authors,
        publisher,
        // `publish_date` is free text ("Jan 01, 2004", "c1998"), so it is
        // reduced here to the year every consumer expects.
        publication_year: book
            .publish_date
            .as_deref()
            .and_then(crate::utils::year::normalize_year),
        cover_url,
        summary,
        page_count: book.number_of_pages,
    }
}

/// Fetch description from Open Library edition and/or work API.
/// Tries edition-level description first, then follows to the parent work.
async fn fetch_description(isbn: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let url = format!("https://openlibrary.org/isbn/{}.json", isbn);
    let edition: serde_json::Value = client.get(&url).send().await.ok()?.json().await.ok()?;

    // Try edition-level description first
    if let Some(desc) = extract_ol_description(&edition) {
        return Some(desc);
    }

    // Follow to work for description
    let work_key = edition
        .get("works")?
        .as_array()?
        .first()?
        .get("key")?
        .as_str()?;
    let work_url = format!("https://openlibrary.org{}.json", work_key);
    let work: serde_json::Value = client.get(&work_url).send().await.ok()?.json().await.ok()?;
    extract_ol_description(&work)
}

/// Extract description from an Open Library JSON response.
/// Handles both plain string and `{type, value}` object formats.
fn extract_ol_description(json: &serde_json::Value) -> Option<String> {
    match json.get("description")? {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Object(obj) => obj
            .get("value")?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => None,
    }
}

pub async fn search_books(query: &str) -> Result<Vec<BookMetadata>, String> {
    let url = format!(
        "https://openlibrary.org/search.json?q={}&limit=10&fields=title,author_name,first_publish_year,cover_i,key,publisher",
        query
    );

    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to send request: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Open Library API error: {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    let parsed: OpenLibrarySearchResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse JSON: {}", e))?;

    let results = parsed
        .docs
        .into_iter()
        .map(|doc| {
            let cover_url = doc
                .cover_i
                .map(|id| format!("https://covers.openlibrary.org/b/id/{}-L.jpg", id));

            let authors = doc
                .author_name
                .unwrap_or_default()
                .into_iter()
                .filter(|name| {
                    let n = name.trim();
                    !n.eq_ignore_ascii_case("unknown author") && !n.eq_ignore_ascii_case("unknown")
                })
                .map(|name| AuthorMetadata {
                    name,
                    birth_year: None,
                    death_year: None,
                    image_url: None,
                    bio: None,
                })
                .collect();

            BookMetadata {
                title: doc.title,
                authors,
                publisher: doc.publisher.and_then(|p| p.first().cloned()),
                publication_year: doc.first_publish_year.map(|y| y.to_string()),
                cover_url,
                summary: None,
                page_count: None,
            }
        })
        .collect();

    Ok(results)
}

#[derive(Debug, Deserialize)]
struct OpenLibrarySearchResponse {
    docs: Vec<OpenLibrarySearchDoc>,
}

#[derive(Debug, Deserialize)]
struct OpenLibrarySearchDoc {
    title: String,
    author_name: Option<Vec<String>>,
    publisher: Option<Vec<String>>,
    first_publish_year: Option<i32>,
    cover_i: Option<i64>,
}

const COVERS_BASE_URL: &str = "https://covers.openlibrary.org";

/// Fetch cover URL from OpenLibrary's Cover API (most reliable endpoint).
/// Uses `?default=false` so OpenLibrary returns 404 for missing covers
/// instead of redirecting to a 1x1 transparent placeholder.
pub async fn fetch_cover_url(isbn: &str) -> Option<String> {
    try_fetch_cover_url(isbn).await.ok().flatten()
}

/// Like [`fetch_cover_url`], but keeps "OpenLibrary has no cover for this ISBN"
/// (a 404, which is an answer) apart from "OpenLibrary did not answer". The
/// cover picker reports the two differently: an outage presented as an absence
/// makes the user give up on a cover that exists.
pub async fn try_fetch_cover_url(isbn: &str) -> Result<Option<String>, String> {
    try_fetch_cover_url_at(COVERS_BASE_URL, isbn).await
}

/// Implementation of [`try_fetch_cover_url`] with an injectable endpoint so the
/// 404 and outage branches can be exercised against a mock server.
async fn try_fetch_cover_url_at(covers_base: &str, isbn: &str) -> Result<Option<String>, String> {
    // Encoded: the ISBN column has no validator, so a hand-typed "/" or "?"
    // would otherwise alter the path or truncate it.
    let cover_url = format!("{}/b/isbn/{}-L.jpg", covers_base, urlencoding::encode(isbn));
    let check_url = format!("{}?default=false", &cover_url);

    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    match client.head(&check_url).send().await {
        Ok(resp) if resp.status().is_success() => Ok(Some(cover_url)),
        // 404 is OpenLibrary telling us it has no cover for this ISBN.
        Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => Ok(None),
        Ok(resp) => Err(format!("HTTP {}", resp.status())),
        Err(e) => Err(format!("Request failed: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── cover lookup: a 404 is an answer, anything else is silence ──────

    #[tokio::test]
    async fn a_404_means_openlibrary_has_no_cover() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("HEAD"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let result = try_fetch_cover_url_at(&server.uri(), "9782073087768").await;

        assert_eq!(result, Ok(None));
    }

    #[tokio::test]
    async fn a_503_is_not_an_absence_of_cover() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("HEAD"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let result = try_fetch_cover_url_at(&server.uri(), "9782073087768").await;

        assert!(result.is_err(), "503 must not read as \"no cover\"");
    }

    // ── ISBN lookup through the Read API ─────────────────────────────────

    /// Real `/api/volumes/brief/isbn/9782752905536.json` answer (Martin Eden,
    /// Phébus 2001 edition OL62528389M), captured 2026-09-22.
    const READ_API_MARTIN_EDEN: &str =
        include_str!("../../../tests/fixtures/openlibrary_volumes_9782752905536.json");

    async fn read_api_server(path: &str, status: u16, body: &str) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(path))
            .respond_with(
                wiremock::ResponseTemplate::new(status)
                    .set_body_raw(body.to_string(), "application/json"),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_known_isbn_yields_its_edition_record() {
        let server = read_api_server(
            "/api/volumes/brief/isbn/9782752905536.json",
            200,
            READ_API_MARTIN_EDEN,
        )
        .await;

        let book = fetch_book_record_at(&server.uri(), "9782752905536")
            .await
            .expect("the server answered")
            .expect("the ISBN is known");

        assert_eq!(book.title, "Martin Eden");
        assert_eq!(book.number_of_pages, Some(454));
        let metadata = metadata_from_book(&book, None);
        assert_eq!(metadata.authors.len(), 1);
        assert_eq!(metadata.authors[0].name, "Jack London");
        assert_eq!(metadata.publisher.as_deref(), Some("Phébus"));
        assert_eq!(metadata.publication_year.as_deref(), Some("2001"));
        assert_eq!(
            metadata.cover_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/15252384-L.jpg")
        );
    }

    /// OpenLibrary answers an unknown ISBN with a bare `[]` and HTTP 200: that
    /// is an absence, not a failure.
    #[tokio::test]
    async fn an_unknown_isbn_is_an_absence_not_a_failure() {
        let server = read_api_server("/api/volumes/brief/isbn/9791234567894.json", 200, "[]").await;

        let result = fetch_book_record_at(&server.uri(), "9791234567894").await;

        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    /// An ISBN-10 query is answered with an edition that lists only its
    /// ISBN-13 (measured on 275290553X): the guard must accept the other form.
    #[tokio::test]
    async fn an_isbn10_query_matches_an_edition_listing_only_its_isbn13() {
        let server = read_api_server(
            "/api/volumes/brief/isbn/275290553X.json",
            200,
            READ_API_MARTIN_EDEN,
        )
        .await;

        let book = fetch_book_record_at(&server.uri(), "275290553X")
            .await
            .expect("the server answered");

        assert_eq!(book.map(|b| b.title).as_deref(), Some("Martin Eden"));
    }

    /// The ISBN is sent as typed (hyphens included, encoded) and matched
    /// against the record in its plain form.
    #[tokio::test]
    async fn a_hyphenated_isbn_is_sent_as_typed_and_still_matches() {
        let server = read_api_server(
            "/api/volumes/brief/isbn/978-2-7529-0553-6.json",
            200,
            READ_API_MARTIN_EDEN,
        )
        .await;

        let book = fetch_book_record_at(&server.uri(), "978-2-7529-0553-6")
            .await
            .expect("the server answered");

        assert_eq!(book.map(|b| b.title).as_deref(), Some("Martin Eden"));
    }

    #[tokio::test]
    async fn a_record_listing_other_isbns_is_discarded() {
        let body = json!({
            "records": {
                "/books/OL1M": {
                    "isbns": ["9780140328721"],
                    "data": { "title": "Fantastic Mr Fox" }
                }
            },
            "items": []
        })
        .to_string();
        let server =
            read_api_server("/api/volumes/brief/isbn/9782752905536.json", 200, &body).await;

        let result = fetch_book_record_at(&server.uri(), "9782752905536").await;

        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    #[tokio::test]
    async fn a_record_without_isbns_cannot_be_checked_and_is_kept() {
        let body = json!({
            "records": {
                "/books/OL1M": { "data": { "title": "Untagged edition" } }
            },
            "items": []
        })
        .to_string();
        let server =
            read_api_server("/api/volumes/brief/isbn/9782752905536.json", 200, &body).await;

        let book = fetch_book_record_at(&server.uri(), "9782752905536")
            .await
            .expect("the server answered");

        assert_eq!(book.map(|b| b.title).as_deref(), Some("Untagged edition"));
    }

    /// Two editions under the same ISBN: the choice is stable across calls.
    #[test]
    fn select_record_is_deterministic_across_hash_orders() {
        let record = |title: &str| {
            serde_json::from_value::<ReadApiRecord>(json!({
                "isbns": ["9782752905536"],
                "data": { "title": title },
            }))
            .expect("fixture should deserialize")
        };
        // Same key → title mapping, inserted in both orders.
        let mut forward = HashMap::new();
        forward.insert("/books/OL1M".to_string(), record("A"));
        forward.insert("/books/OL2M".to_string(), record("B"));
        let mut backward = HashMap::new();
        backward.insert("/books/OL2M".to_string(), record("B"));
        backward.insert("/books/OL1M".to_string(), record("A"));

        let a = select_record(forward, "9782752905536").map(|r| r.title);
        let b = select_record(backward, "9782752905536").map(|r| r.title);

        assert_eq!(a.as_deref(), Some("A"), "lowest key wins");
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn a_404_from_the_read_api_is_a_failure_not_an_absence() {
        let server = read_api_server("/api/volumes/brief/isbn/9782752905536.json", 404, "").await;

        let result = fetch_book_record_at(&server.uri(), "9782752905536").await;

        assert!(result.is_err(), "404 must not read as \"unknown ISBN\"");
    }

    #[tokio::test]
    async fn a_503_from_the_read_api_is_a_failure() {
        let server = read_api_server("/api/volumes/brief/isbn/9782752905536.json", 503, "").await;

        assert!(
            fetch_book_record_at(&server.uri(), "9782752905536")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_body_that_is_not_the_envelope_is_a_failure() {
        let server = read_api_server(
            "/api/volumes/brief/isbn/9782752905536.json",
            200,
            "<html>maintenance</html>",
        )
        .await;

        assert!(
            fetch_book_record_at(&server.uri(), "9782752905536")
                .await
                .is_err()
        );
    }

    #[test]
    fn test_extract_ol_description_plain_string() {
        let data = json!({ "description": "A classic novel about identity." });
        assert_eq!(
            extract_ol_description(&data),
            Some("A classic novel about identity.".to_string())
        );
    }

    #[test]
    fn test_extract_ol_description_typed_object() {
        let data = json!({
            "description": {
                "type": "/type/text",
                "value": "An epic tale of adventure."
            }
        });
        assert_eq!(
            extract_ol_description(&data),
            Some("An epic tale of adventure.".to_string())
        );
    }

    #[test]
    fn test_extract_ol_description_empty_string_returns_none() {
        let data = json!({ "description": "" });
        assert_eq!(extract_ol_description(&data), None);
    }

    #[test]
    fn test_extract_ol_description_empty_value_returns_none() {
        let data = json!({ "description": { "type": "/type/text", "value": "" } });
        assert_eq!(extract_ol_description(&data), None);
    }

    #[test]
    fn test_extract_ol_description_missing_field_returns_none() {
        let data = json!({ "title": "Some book" });
        assert_eq!(extract_ol_description(&data), None);
    }

    #[test]
    fn test_extract_ol_description_unexpected_type_returns_none() {
        let data = json!({ "description": 42 });
        assert_eq!(extract_ol_description(&data), None);
    }

    fn ol_book(value: serde_json::Value) -> OpenLibraryBook {
        serde_json::from_value(value).expect("fixture should deserialize")
    }

    /// OpenLibrary answers a free-text `publish_date`. Reading its first four
    /// characters offered the reader a month prefix ("Jan ") as a year.
    #[test]
    fn metadata_reduces_a_free_text_publish_date_to_its_year() {
        let book = ol_book(json!({
            "title": "L'étranger",
            "publish_date": "Jan 01, 2004",
            "publishers": [{ "name": "Gallimard" }],
            "number_of_pages": 186
        }));

        let metadata = metadata_from_book(&book, None);

        assert_eq!(metadata.publication_year.as_deref(), Some("2004"));
        // The neighbouring fields prove the mapping is not merely shuffled.
        assert_eq!(metadata.publisher.as_deref(), Some("Gallimard"));
        assert_eq!(metadata.page_count, Some(186));
    }

    #[test]
    fn metadata_reports_no_year_when_the_date_holds_none() {
        let book = ol_book(json!({ "title": "T", "publish_date": "unknown" }));
        assert_eq!(metadata_from_book(&book, None).publication_year, None);
    }

    #[test]
    fn metadata_reports_no_year_when_the_date_is_absent() {
        let book = ol_book(json!({ "title": "T" }));
        assert_eq!(metadata_from_book(&book, None).publication_year, None);
    }
}
