use super::API_USER_AGENT;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GoogleBooksResponse {
    items: Option<Vec<GoogleBookItem>>,
}

#[derive(Debug, Deserialize)]
struct GoogleBookItem {
    #[serde(rename = "volumeInfo")]
    volume_info: GoogleVolumeInfo,
}

#[derive(Debug, Deserialize)]
struct GoogleVolumeInfo {
    title: String,
    authors: Option<Vec<String>>,
    publisher: Option<String>,
    #[serde(rename = "publishedDate")]
    published_date: Option<String>,
    description: Option<String>,
    language: Option<String>,
    #[serde(rename = "pageCount")]
    page_count: Option<u32>,
    #[serde(rename = "imageLinks")]
    image_links: Option<GoogleImageLinks>,
    #[serde(rename = "industryIdentifiers")]
    industry_identifiers: Option<Vec<GoogleIndustryIdentifier>>,
}

#[derive(Debug, Deserialize)]
struct GoogleIndustryIdentifier {
    #[serde(rename = "type")]
    id_type: String,
    identifier: String,
}

#[derive(Debug, Deserialize)]
struct GoogleImageLinks {
    thumbnail: Option<String>,
    // smallThumbnail is also available but often too small
}

// Reuse definitions from openlibrary or define local mapping
use crate::inventaire_client::AuthorMetadata;
use crate::openlibrary::BookMetadata;

/// Build a Google Books API URL, appending the API key if provided.
fn append_api_key(url: &str, api_key: Option<&str>) -> String {
    match api_key.filter(|k| !k.is_empty()) {
        Some(key) => format!("{}&key={}", url, key),
        None => url.to_string(),
    }
}

/// Whether the caller can afford a second attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    /// Someone is waiting on the answer, so a 5xx is worth one more try: the
    /// alternative is telling them a source is down when it merely hiccuped.
    Interactive,
    /// A background sweep over the whole library. A second try per failing book
    /// buys almost nothing there (the sweep already reports what it found) and
    /// costs a fixed pause on every one of them at each app start.
    Background,
}

/// The `reason` Google put in an error body, for the log line that reports the
/// status.
///
/// A bare "HTTP 503" cannot be acted on: Google spells throttling
/// (`rateLimitExceeded`, `userRateLimitExceeded`) and a genuine outage
/// (`backendError`) with the same status code, and only the body tells them
/// apart. The envelope carries no credential — the API key travels in the query
/// string, which is never echoed back — so this is safe to log.
async fn error_reason(resp: reqwest::Response) -> String {
    let Ok(body) = resp.text().await else {
        return "body unreadable".to_string();
    };
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/errors/0/reason")
                .and_then(|r| r.as_str())
                .map(str::to_string)
        })
        // Truncated: an HTML error page from a proxy would otherwise fill the log.
        .unwrap_or_else(|| {
            format!(
                "no reason field: {}",
                body.chars().take(120).collect::<String>()
            )
        })
}

/// GET, retrying once on a 5xx when the policy allows it.
///
/// Google answers 503 in bursts, and the code used to give up on the first one:
/// the only source that holds a cover for a French edition absent from
/// Inventaire and OpenLibrary would then report itself unavailable, and the
/// reader saw a search that had in fact never reached it. 503 is by definition
/// "come back", so we come back once. A 429 is NEVER retried, whatever the
/// policy: the quota is spent, and asking again only digs deeper.
async fn get_with_one_retry(
    client: &reqwest::Client,
    url: &str,
    policy: RetryPolicy,
) -> Result<reqwest::Response, String> {
    let first = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", redact(e)))?;
    if policy == RetryPolicy::Background || !first.status().is_server_error() {
        return Ok(first);
    }
    // Warn, not debug: the default filter is `info`, and a silent retry makes
    // the failure that follows it unreadable in a log.
    tracing::warn!("Google Books: {}, retrying once", first.status());
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", redact(e)))
}

/// Strip the URL out of a transport error before it is logged or handed back.
///
/// [`append_api_key`] puts the user's key in the query string, and a
/// `reqwest::Error` renders the whole URL in its `Display`. That message reaches
/// the log file and, since covers report per-source failures, the Flutter side
/// too. The rest of the error (timeout, connect, decode) is what diagnoses the
/// problem anyway; the URL never was.
fn redact(e: reqwest::Error) -> reqwest::Error {
    e.without_url()
}

pub async fn fetch_book_metadata(
    isbn: &str,
    api_key: Option<&str>,
) -> Result<BookMetadata, String> {
    let base_url = format!(
        "https://www.googleapis.com/books/v1/volumes?q=isbn:{}",
        isbn
    );
    let url = append_api_key(&base_url, api_key);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;

    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let msg = if api_key.is_none() {
            "Google Books API quota exceeded (no API key configured). Add your own key in Settings to fix this."
        } else {
            "Google Books API quota exceeded for your API key"
        };
        tracing::warn!("{}", msg);
        return Err(msg.to_string());
    }
    if !status.is_success() {
        tracing::warn!("Google Books API error for ISBN {}: HTTP {}", isbn, status);
        return Err(format!("Google Books API Error: {}", status));
    }

    let body = resp.text().await.map_err(|e| e.to_string())?;
    let parsed: GoogleBooksResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    if let Some(items) = parsed.items
        && let Some(first_item) = items.first()
    {
        return Ok(metadata_from_volume_info(&first_item.volume_info));
    }

    Err("Book not found in Google Books".to_string())
}

/// Project a Google Books volume onto the shared metadata shape.
///
/// Split from the fetch so the field mapping stays testable without a network
/// call: `publishedDate` is an ISO date as often as a bare year, and a
/// mis-wired field would go unnoticed here.
fn metadata_from_volume_info(info: &GoogleVolumeInfo) -> BookMetadata {
    let authors = info
        .authors
        .as_ref()
        .map(|list| {
            list.iter()
                .filter(|name| {
                    let n = name.trim();
                    !n.eq_ignore_ascii_case("unknown author") && !n.eq_ignore_ascii_case("unknown")
                })
                .map(|name| AuthorMetadata {
                    name: name.clone(),
                    birth_year: None,
                    death_year: None,
                    image_url: None,
                    bio: None,
                })
                .collect()
        })
        .unwrap_or_default();

    let cover_url = info
        .image_links
        .as_ref()
        .and_then(|l| l.thumbnail.clone())
        .map(|url| url.replace("http://", "https://"));

    BookMetadata {
        title: info.title.clone(),
        authors,
        publisher: info.publisher.clone(),
        publication_year: info
            .published_date
            .as_deref()
            .and_then(crate::utils::year::normalize_year),
        cover_url,
        summary: info.description.clone(),
        page_count: info.page_count,
    }
}

const GOOGLE_BOOKS_VOLUMES_URL: &str = "https://www.googleapis.com/books/v1/volumes";

pub async fn fetch_cover_url(isbn: &str, api_key: Option<&str>) -> Option<String> {
    try_fetch_cover_url_background(isbn, api_key)
        .await
        .ok()
        .flatten()
}

/// Like [`fetch_cover_url`], and keeps "Google has no cover for this ISBN" apart
/// from "Google did not answer", but stays on the background retry policy.
///
/// The startup sweep needs the distinction — it must not record an outage as
/// "no cover exists" — without the retry: it walks every coverless book, so a
/// fixed pause per failure is paid once per book.
pub async fn try_fetch_cover_url_background(
    isbn: &str,
    api_key: Option<&str>,
) -> Result<Option<String>, String> {
    try_fetch_cover_url_at(
        GOOGLE_BOOKS_VOLUMES_URL,
        isbn,
        api_key,
        RetryPolicy::Background,
    )
    .await
}

/// Like [`fetch_cover_url`], but keeps "Google has no cover for this ISBN" apart
/// from "Google did not answer" (transport error, 5xx, or a saturated quota).
/// The cover picker reports the two differently: an outage presented as an
/// absence makes the user give up on a cover that exists.
pub async fn try_fetch_cover_url(
    isbn: &str,
    api_key: Option<&str>,
) -> Result<Option<String>, String> {
    try_fetch_cover_url_at(
        GOOGLE_BOOKS_VOLUMES_URL,
        isbn,
        api_key,
        RetryPolicy::Interactive,
    )
    .await
}

/// Implementation of [`try_fetch_cover_url`] with an injectable endpoint so the
/// quota and outage branches can be exercised against a mock server.
async fn try_fetch_cover_url_at(
    volumes_url: &str,
    isbn: &str,
    api_key: Option<&str>,
    policy: RetryPolicy,
) -> Result<Option<String>, String> {
    // Encoded: the ISBN column has no validator, so a hand-typed "&" or "#"
    // would otherwise inject a parameter or truncate the query.
    let base_url = format!("{}?q=isbn:{}", volumes_url, urlencoding::encode(isbn));
    let url = append_api_key(&base_url, api_key);

    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    let resp = get_with_one_retry(&client, &url, policy)
        .await
        .inspect_err(|e| {
            // Logged like every other failure branch: this one used to be the only
            // silent one, so an unreachable Google left no trace at all.
            tracing::warn!("Google Books cover fetch failed for ISBN {}: {}", isbn, e);
        })?;

    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        tracing::warn!(
            "Google Books API quota exceeded fetching cover for ISBN {}{}",
            isbn,
            if api_key.is_none() {
                " (no API key)"
            } else {
                ""
            }
        );
        return Err(QUOTA_FAILURE.to_string());
    }
    if !status.is_success() {
        tracing::warn!(
            "Google Books cover fetch error for ISBN {}: HTTP {} ({})",
            isbn,
            status,
            error_reason(resp).await
        );
        return Err(format!("HTTP {}", status));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Read body failed: {}", e))?;
    let parsed: GoogleBooksResponse =
        serde_json::from_str(&body).map_err(|e| format!("Parse error: {}", e))?;

    if let Some(items) = parsed.items
        && let Some(first_item) = items.first()
        && let Some(links) = &first_item.volume_info.image_links
        && let Some(thumb) = &links.thumbnail
    {
        // Google Books returns http links often, upgrade to https
        let secure_url = thumb.replace("http://", "https://");
        return Ok(Some(secure_url));
    }

    // A volume without an image, or no volume at all, is a real answer.
    Ok(None)
}

/// Outcome of a Google Books search.
///
/// `quota_exceeded` is set when Google answers HTTP 429. Without an API key all
/// anonymous requests share a single global Google project whose daily quota is
/// routinely saturated, so an empty `books` list is otherwise indistinguishable
/// from "no match". Callers use this flag to surface an honest "limite atteinte"
/// notice instead of a silent empty result.
///
/// `failure` carries the same distinction for everything that is not a quota:
/// a transport error, a 5xx, or a body we cannot parse. Without it an outage is
/// indistinguishable from "Google knows nothing about this book".
#[derive(Debug, Default)]
pub struct GoogleBooksSearchResult {
    pub books: Vec<crate::models::book::Model>,
    pub quota_exceeded: bool,
    pub failure: Option<String>,
}

/// Marker for a saturated Google quota, so callers can name it rather than
/// showing a bare HTTP code.
pub const QUOTA_FAILURE: &str = "quota";

pub async fn search_books(
    query: &crate::api::search::SearchQuery,
    api_key: Option<&str>,
) -> GoogleBooksSearchResult {
    search_books_at(GOOGLE_BOOKS_VOLUMES_URL, query, api_key).await
}

/// Implementation of [`search_books`] with an injectable endpoint so the
/// quota/parse branches can be exercised against a mock server in tests.
async fn search_books_at(
    volumes_url: &str,
    query: &crate::api::search::SearchQuery,
    api_key: Option<&str>,
) -> GoogleBooksSearchResult {
    let mut result = GoogleBooksSearchResult::default();
    let mut q_parts = Vec::new();

    if let Some(q) = &query.q {
        q_parts.push(urlencoding::encode(q).to_string());
    } else {
        if let Some(t) = &query.title {
            q_parts.push(format!("intitle:{}", urlencoding::encode(t)));
        }
        if let Some(a) = &query.author {
            q_parts.push(format!("inauthor:{}", urlencoding::encode(a)));
        }
        if let Some(p) = &query.publisher {
            q_parts.push(format!("inpublisher:{}", urlencoding::encode(p)));
        }
        if let Some(s) = &query.subjects {
            q_parts.push(format!("subject:{}", urlencoding::encode(s)));
        }
    }

    if q_parts.is_empty() {
        return result;
    }

    let q_str = q_parts.join("+"); // Google Books uses + or space
    let max_results = if query.autocomplete.unwrap_or(false) {
        10 // More results for autocomplete to allow quality filtering
    } else {
        15
    };
    let base_url = format!("{}?q={}&maxResults={}", volumes_url, q_str, max_results);
    let url = append_api_key(&base_url, api_key);

    let client = reqwest::Client::builder()
        .user_agent(API_USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = match get_with_one_retry(&client, &url, RetryPolicy::Interactive).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Google Books search request failed: {}", e);
            result.failure = Some(e);
            return result;
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        if api_key.is_none() {
            tracing::warn!(
                "Google Books search quota exceeded (no API key configured). Add your own key in Settings."
            );
        } else {
            tracing::warn!("Google Books search quota exceeded for your API key");
        }
        result.quota_exceeded = true;
        result.failure = Some(QUOTA_FAILURE.to_string());
        return result;
    }
    if !status.is_success() {
        tracing::warn!("Google Books search error: HTTP {}", status);
        result.failure = Some(format!("HTTP {}", status));
        return result;
    }

    let parsed = match resp.json::<GoogleBooksResponse>().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Google Books response parse error: {}", e);
            result.failure = Some(format!("Parse error: {}", e));
            return result;
        }
    };

    if let Some(items) = parsed.items {
        for item in items {
            let info = item.volume_info;

            // Convert to Book Model
            let cover_url = info
                .image_links
                .as_ref()
                .and_then(|l| l.thumbnail.clone())
                .map(|url| url.replace("http://", "https://"));

            // Extract ISBN from industryIdentifiers (prefer ISBN_13 over ISBN_10)
            let isbn = info.industry_identifiers.as_ref().and_then(|ids| {
                // First try to find ISBN_13
                ids.iter()
                    .find(|id| id.id_type == "ISBN_13")
                    .or_else(|| ids.iter().find(|id| id.id_type == "ISBN_10"))
                    .map(|id| id.identifier.replace("-", ""))
            });

            if info.industry_identifiers.is_none() {
                tracing::debug!("Google Books: no industryIdentifiers for '{}'", info.title);
            }

            let source_data = serde_json::json!({
               "source": "google_books",
               "authors": info.authors.clone().unwrap_or_default(),
               "language": info.language.clone(),
            });

            let book = crate::models::book::Model {
                id: String::new(), // transient search result, never persisted
                title: info.title,
                isbn,
                publisher: info.publisher,
                publication_year: info
                    .published_date
                    .as_deref()
                    .and_then(crate::utils::year::parse_year),
                summary: info.description,
                dewey_decimal: None,
                lcc: None,
                subjects: None,
                marc_record: None,
                cataloguing_notes: None,
                source_data: Some(source_data.to_string()),
                shelf_position: None,
                cover_url,
                reading_status: "to_read".to_string(),
                finished_reading_at: None,
                started_reading_at: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                user_rating: None,
                owned: true,
                price: None,
                digital_formats: None,
                private: false,
                page_count: info.page_count.map(|p| p as i32),
                loan_duration_days: None,
            };
            result.books.push(book);
        }
    }

    // Deduplicate results - Google Books often returns the same book multiple times
    // (different formats like hardcover/paperback/ebook with identical data)
    let mut seen = std::collections::HashSet::new();
    result.books.retain(|book| {
        // Create dedup key: prefer ISBN, fallback to title+publisher+year
        let key = if let Some(ref isbn) = book.isbn {
            isbn.clone()
        } else {
            format!(
                "{}|{}|{}",
                book.title.to_lowercase(),
                book.publisher.as_deref().unwrap_or("").to_lowercase(),
                book.publication_year.unwrap_or(0)
            )
        };
        seen.insert(key)
    });

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn query(q: &str) -> crate::api::search::SearchQuery {
        crate::api::search::SearchQuery {
            title: None,
            author: None,
            publisher: None,
            year_min: None,
            year_max: None,
            tags: None,
            q: Some(q.to_string()),
            subjects: None,
            sources: None,
            autocomplete: None,
        }
    }

    // ── cover lookup: absence, quota and outage are three answers ───────

    // ── Google answers 503 in bursts; one 503 is not an outage ──────────

    #[tokio::test]
    async fn a_503_is_retried_once_and_the_second_answer_counts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{ "volumeInfo": { "title": "Mécanismes de survie en milieu hostile",
                                            "imageLinks": { "thumbnail": "http://books.google.com/x" } } }]
            })))
            .with_priority(2)
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result =
            try_fetch_cover_url_at(&url, "9782070468287", None, RetryPolicy::Interactive).await;

        assert_eq!(result, Ok(Some("https://books.google.com/x".to_string())));
    }

    #[tokio::test]
    async fn the_startup_sweep_does_not_retry() {
        // `enrich_missing_covers` walks every coverless book at each app start.
        // A retry per failure there adds a fixed pause to each one and buys
        // nothing: nobody is waiting on that answer.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result =
            try_fetch_cover_url_at(&url, "9782073087768", None, RetryPolicy::Background).await;

        assert!(result.is_err(), "503 is still not an absence");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the background sweep must ask exactly once"
        );
    }

    #[tokio::test]
    async fn a_spent_quota_is_never_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let _ = try_fetch_cover_url_at(&url, "9782070468287", None, RetryPolicy::Interactive).await;

        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "asking again only digs the quota deeper"
        );
    }

    #[tokio::test]
    async fn google_is_told_who_is_calling() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let _ = try_fetch_cover_url_at(&url, "9782070468287", None, RetryPolicy::Interactive).await;

        let received = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            received
                .headers
                .get("user-agent")
                .map(|v| v.to_str().unwrap()),
            Some(API_USER_AGENT),
            "every other integration identifies itself; this one did not"
        );
    }

    // ── the API key must not ride along in what we log or return ────────
    //
    // `append_api_key` puts the key in the query string, and a reqwest error
    // renders the full URL. Covers now report per-source failures to Flutter,
    // so that message leaves the log file: it must carry no key.

    #[tokio::test]
    async fn a_transport_error_carries_no_api_key() {
        // Port 1 is closed: this fails at connect, the branch that renders a URL.
        let unreachable = "http://127.0.0.1:1/books/v1/volumes";
        let key = "AIzaTESTKEYTESTKEYTESTKEY";

        let cover = try_fetch_cover_url_at(
            unreachable,
            "9782073087768",
            Some(key),
            RetryPolicy::Interactive,
        )
        .await;
        let search = search_books_at(unreachable, &query("Retour en Afrique"), Some(key)).await;

        let cover_err = cover.expect_err("a closed port must fail");
        assert!(
            !cover_err.contains(key),
            "cover lookup leaked the API key: {cover_err}"
        );
        let search_err = search.failure.expect("a closed port must fail");
        assert!(
            !search_err.contains(key),
            "search leaked the API key: {search_err}"
        );
    }

    // ── the ISBN column has no validator, so it must be encoded ─────────

    #[tokio::test]
    async fn a_hand_typed_isbn_cannot_inject_a_query_parameter() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let _ =
            try_fetch_cover_url_at(&url, "978&key=stolen", None, RetryPolicy::Interactive).await;

        let received = &server.received_requests().await.unwrap()[0];
        let query = received.url.query().unwrap_or_default();
        assert!(
            !query.contains("&key="),
            "the ISBN injected a parameter: {query}"
        );
        assert!(
            query.contains("%26"),
            "the ampersand should have been encoded: {query}"
        );
    }

    #[tokio::test]
    async fn a_volume_without_an_image_is_an_absence_not_a_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{ "volumeInfo": { "title": "Retour en Afrique" } }]
            })))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result =
            try_fetch_cover_url_at(&url, "9782073087768", None, RetryPolicy::Interactive).await;

        assert_eq!(result, Ok(None));
    }

    #[tokio::test]
    async fn a_503_cover_lookup_is_not_an_absence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result =
            try_fetch_cover_url_at(&url, "9782073087768", None, RetryPolicy::Interactive).await;

        assert!(result.is_err(), "503 must not read as \"no cover\"");
    }

    #[tokio::test]
    async fn a_saturated_quota_names_itself_on_the_cover_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result =
            try_fetch_cover_url_at(&url, "9782073087768", None, RetryPolicy::Interactive).await;

        assert_eq!(result, Err(QUOTA_FAILURE.to_string()));
    }

    #[tokio::test]
    async fn a_search_outage_is_reported_as_a_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result = search_books_at(&url, &query("Retour en Afrique"), None).await;

        assert!(result.books.is_empty());
        assert!(
            result.failure.is_some(),
            "an empty book list after a 503 must carry the failure"
        );
    }

    #[tokio::test]
    async fn quota_exceeded_flag_set_on_429() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result = search_books_at(&url, &query("Martin Eden"), None).await;

        assert!(result.quota_exceeded, "429 must set quota_exceeded");
        assert!(result.books.is_empty(), "no books on quota error");
    }

    #[tokio::test]
    async fn quota_flag_clear_on_success_with_items() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "items": [{
                "volumeInfo": {
                    "title": "Martin Eden",
                    "authors": ["Jack London"],
                    "industryIdentifiers": [
                        {"type": "ISBN_13", "identifier": "9782070123456"}
                    ]
                }
            }]
        });
        Mock::given(method("GET"))
            .and(path("/books/v1/volumes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let url = format!("{}/books/v1/volumes", server.uri());
        let result = search_books_at(&url, &query("Martin Eden"), None).await;

        assert!(!result.quota_exceeded, "200 must not set quota_exceeded");
        assert_eq!(result.books.len(), 1);
        assert_eq!(result.books[0].title, "Martin Eden");
    }

    #[tokio::test]
    async fn quota_flag_clear_on_empty_query() {
        // An empty query never hits the network, so it is "no match", not a quota error.
        let empty = crate::api::search::SearchQuery {
            title: None,
            author: None,
            publisher: None,
            year_min: None,
            year_max: None,
            tags: None,
            q: None,
            subjects: None,
            sources: None,
            autocomplete: None,
        };
        let result = search_books_at("http://127.0.0.1:0/books/v1/volumes", &empty, None).await;
        assert!(!result.quota_exceeded);
        assert!(result.books.is_empty());
    }

    fn volume_info(value: serde_json::Value) -> GoogleVolumeInfo {
        serde_json::from_value(value).expect("fixture should deserialize")
    }

    /// Google answers `publishedDate` as an ISO date as often as a bare year.
    /// Parsing the whole string as an integer dropped every dated one.
    #[test]
    fn metadata_reduces_an_iso_published_date_to_its_year() {
        let info = volume_info(json!({
            "title": "The Stranger",
            "publishedDate": "2004-01-01",
            "publisher": "Vintage",
            "pageCount": 123
        }));

        let metadata = metadata_from_volume_info(&info);

        assert_eq!(metadata.publication_year.as_deref(), Some("2004"));
        // The neighbouring fields prove the mapping is not merely shuffled.
        assert_eq!(metadata.publisher.as_deref(), Some("Vintage"));
        assert_eq!(metadata.page_count, Some(123));
    }

    #[test]
    fn metadata_keeps_a_bare_published_year() {
        let info = volume_info(json!({ "title": "T", "publishedDate": "1998" }));
        assert_eq!(
            metadata_from_volume_info(&info).publication_year.as_deref(),
            Some("1998")
        );
    }

    #[test]
    fn metadata_reports_no_year_when_the_date_is_absent() {
        let info = volume_info(json!({ "title": "T" }));
        assert_eq!(metadata_from_volume_info(&info).publication_year, None);
    }
}
