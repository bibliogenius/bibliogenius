use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use serde_json::json;
use std::sync::LazyLock;
use strsim::jaro_winkler;

use crate::models::book;
use crate::modules::integrations::sudoc;
use futures::stream::{self, StreamExt};

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub isbn: String,
}

pub async fn search_sudoc(
    State(_db): State<DatabaseConnection>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    match sudoc::fetch_by_isbn(&params.isbn).await {
        Ok(book) => (
            StatusCode::OK,
            Json(json!({ "success": true, "book": book })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "error": e })),
        )
            .into_response(),
    }
}

// --- Federated Search Helpers ---

#[derive(Deserialize, Debug)]
struct OpenLibrarySearchResponse {
    docs: Vec<OpenLibraryDoc>,
}

#[derive(Deserialize, Debug)]
struct OpenLibraryDoc {
    title: String,
    author_name: Option<Vec<String>>,
    first_publish_year: Option<i32>,
    #[serde(alias = "publishers")]
    publisher: Option<Vec<String>>,
    isbn: Option<Vec<String>>,
    cover_i: Option<i32>,
    language: Option<Vec<String>>,
    edition_key: Option<Vec<String>>, // For fetching ISBN from editions
    edition_count: Option<i32>,       // Number of editions (popularity signal)
    key: String,                      // Work ID (e.g. "/works/OL12345W")
}

/// Strip regional/country suffix from a BCP 47 tag: "pt-BR" → "pt", "zh-TW" → "zh".
/// Already-simple codes like "fr" pass through unchanged.
fn base_lang(code: &str) -> &str {
    code.split(['-', '_']).next().unwrap_or(code)
}

// Helper to check if language matches (handles 2-letter vs 3-letter codes and regional variants)
fn lang_matches(book_lang: &str, user_lang: &str) -> bool {
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

/// Check if a book language matches ANY of the user's preferred languages
fn lang_matches_any(book_lang: &str, user_langs: &[String]) -> bool {
    if user_langs.is_empty() {
        return true;
    }
    user_langs.iter().any(|ul| lang_matches(book_lang, ul))
}

fn normalize_string(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'î' | 'ï' => 'i',
            'ó' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'û' | 'ù' | 'ü' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'ş' => 's',
            'ğ' => 'g',
            'ı' => 'i',
            'ø' => 'o',
            'æ' => 'a',
            'ý' => 'y',
            _ => c,
        })
        .collect()
}

/// Fetch ISBN and Publisher from OpenLibrary edition API when search results don't include them
async fn fetch_edition_extras(edition_key: &str) -> Option<(String, Option<String>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    let url = format!("https://openlibrary.org/books/{}.json", edition_key);

    #[derive(serde::Deserialize)]
    struct EditionResponse {
        isbn_13: Option<Vec<String>>,
        isbn_10: Option<Vec<String>>,
        publishers: Option<Vec<String>>,
    }

    if let Ok(res) = client.get(&url).send().await
        && let Ok(edition) = res.json::<EditionResponse>().await
    {
        let publisher = edition.publishers.and_then(|v| v.first().cloned());

        // Prefer ISBN-13 over ISBN-10
        if let Some(isbns) = edition.isbn_13
            && let Some(isbn) = isbns.first()
        {
            return Some((isbn.clone(), publisher));
        }
        if let Some(isbns) = edition.isbn_10
            && let Some(isbn) = isbns.first()
        {
            return Some((isbn.clone(), publisher));
        }
    }
    None
}

/// Fetch ISBN and Publisher from OpenLibrary Work API (get first edition)
async fn fetch_work_edition_extras(work_key: &str) -> Option<(String, Option<String>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .ok()?;

    // Fetch up to 5 editions to find one with an ISBN
    let url = format!("https://openlibrary.org{}/editions.json?limit=5", work_key);

    #[derive(serde::Deserialize)]
    struct WorkEditionsResponse {
        entries: Vec<EditionEntry>,
    }

    #[derive(serde::Deserialize)]
    struct EditionEntry {
        isbn_13: Option<Vec<String>>,
        isbn_10: Option<Vec<String>>,
        publishers: Option<Vec<String>>,
    }

    if let Ok(res) = client.get(&url).send().await
        && let Ok(data) = res.json::<WorkEditionsResponse>().await
    {
        // Iterate over entries to find the first one with an ISBN
        for entry in data.entries {
            let publisher = entry.publishers.and_then(|v| v.first().cloned());

            // Prefer ISBN-13
            if let Some(isbns) = &entry.isbn_13
                && let Some(isbn) = isbns.first()
            {
                return Some((isbn.clone(), publisher));
            }
            if let Some(isbns) = &entry.isbn_10
                && let Some(isbn) = isbns.first()
            {
                return Some((isbn.clone(), publisher));
            }
        }
    }
    None
}

pub async fn search_external(
    query: &crate::api::search::SearchQuery,
    db: &DatabaseConnection,
) -> Vec<book::Model> {
    // Check if OpenLibrary fallback is enabled
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    let enable_openlibrary = match ProfileEntity::find_by_id(1).one(db).await {
        Ok(Some(profile_model)) => {
            let modules: Vec<String> =
                serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
            !modules.contains(&"disable_fallback:openlibrary".to_string())
        }
        _ => true,
    };

    let mut books = Vec::new();

    if enable_openlibrary {
        // Add timeout to prevent hanging when OpenLibrary is down
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // Build Open Library Query
        // Always use `q=` (general search) for OpenLibrary because field-specific
        // searches like `title:X` only match exact titles and miss translations
        // (e.g. "Le tunnel" won't find "El túnel" by Sabato with title: but will with q=).
        let mut q_terms = Vec::new();

        if let Some(q) = &query.q {
            q_terms.push(q.clone());
        } else {
            if let Some(t) = &query.title {
                q_terms.push(t.clone());
            }
            if let Some(a) = &query.author {
                q_terms.push(a.clone());
            }
            if let Some(s) = &query.subjects {
                q_terms.push(s.clone());
            }
        }

        if q_terms.is_empty() {
            return books;
        }

        let limit_val = if query.autocomplete.unwrap_or(false) {
            12 // More results for autocomplete to allow quality filtering
        } else {
            20
        };
        let url = format!(
            "https://openlibrary.org/search.json?q={}&limit={}",
            urlencoding::encode(&q_terms.join(" ")),
            limit_val
        );

        if let Ok(res) = client.get(&url).send().await {
            match res.json::<OpenLibrarySearchResponse>().await {
                Ok(data) => {
                    for doc in data.docs {
                        let isbn = doc.isbn.as_ref().and_then(|v| v.first()).cloned();

                        // Map to our Book Model (store additional data in source_data)
                        let source_data = serde_json::json!({
                            "authors": doc.author_name.clone().unwrap_or_default(),
                            "cover_id": doc.cover_i,
                            "source": "openlibrary",
                            "languages": doc.language.clone().unwrap_or_default(),
                            "isbns": doc.isbn.clone().unwrap_or_default(),
                            "edition_key": doc.edition_key.as_ref().and_then(|k| k.first()).cloned(),
                            "edition_count": doc.edition_count,
                            "key": doc.key,
                            "publisher": doc.publisher.clone().unwrap_or_default()
                        });

                        let cover_url = doc
                            .cover_i
                            .map(|id| format!("https://covers.openlibrary.org/b/id/{}-M.jpg", id));

                        let book = book::Model {
                            id: 0, // Placeholder ID
                            title: doc.title,
                            isbn,
                            publisher: doc.publisher.map(|v| v.join(", ")),
                            publication_year: doc.first_publish_year,
                            summary: None,
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
                            owned: true, // External search results are assumed owned
                            price: None, // No price from external search
                            digital_formats: None,
                        };
                        books.push(book);
                    }
                }
                Err(_e) => {}
            }
        }
    }
    books
}

/// Public endpoint for Open Library search (proxy to avoid CORS issues)
#[derive(Deserialize)]
pub struct OpenLibraryQuery {
    title: Option<String>,
    author: Option<String>,
    subject: Option<String>,
}

pub async fn search_openlibrary(Query(params): Query<OpenLibraryQuery>) -> impl IntoResponse {
    let client = reqwest::Client::new();

    // Build query parameters for Open Library
    let mut query_params = vec![("limit", "20".to_string())];

    if let Some(title) = params.title {
        query_params.push(("title", title));
    }
    if let Some(author) = params.author {
        query_params.push(("author", author));
    }
    if let Some(subject) = params.subject {
        query_params.push(("subject", subject));
    }

    // Call Open Library API
    match client
        .get("https://openlibrary.org/search.json")
        .query(&query_params)
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<serde_json::Value>().await {
                    Ok(data) => (StatusCode::OK, Json(data)).into_response(),
                    Err(_) => (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "Failed to parse response" })),
                    )
                        .into_response(),
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Open Library returned error" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact Open Library" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct UnifiedSearchQuery {
    pub q: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub subject: Option<String>,
    pub lang: Option<String>, // User's preferred language (e.g., "fr", "en")
    pub source: Option<String>, // Filter to specific source: "inventaire", "bnf", "openlibrary", "google_books" or comma-separated
    pub autocomplete: Option<bool>,
}

pub async fn search_unified(
    State(db): State<DatabaseConnection>,
    Query(params): Query<UnifiedSearchQuery>,
) -> impl IntoResponse {
    let search_start = std::time::Instant::now();
    let mut results: Vec<book::Book> = Vec::new();

    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    // Load profile config to check enabled providers and API keys
    let (
        mut enable_inventaire,
        enable_bnf,
        mut enable_openlibrary,
        mut enable_google_books,
        google_books_api_key,
    ) = match ProfileEntity::find_by_id(1).one(&db).await {
        Ok(Some(profile_model)) => {
            let modules: Vec<String> =
                serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
            let inv = !modules.contains(&"disable_fallback:inventaire".to_string());
            let bnf = !modules.contains(&"disable_fallback:bnf".to_string());
            let ol = !modules.contains(&"disable_fallback:openlibrary".to_string());
            let gb = modules.contains(&"enable_google_books".to_string());
            let api_keys: std::collections::HashMap<String, String> = profile_model
                .api_keys
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let gb_key = api_keys.get("google_books").cloned();
            (inv, bnf, ol, gb, gb_key)
        }
        _ => (true, true, true, false, None),
    };

    let is_autocomplete = params.autocomplete.unwrap_or(false);
    let mut search_timeout = std::time::Duration::from_secs(8);

    // Separate BNF flags: SPARQL (slow) vs SRU (fast, better metadata)
    let mut enable_bnf_sparql = enable_bnf;
    let mut enable_bnf_sru = enable_bnf;

    if is_autocomplete {
        // Always disable slow BNF SPARQL in autocomplete
        enable_bnf_sparql = false;
        search_timeout = std::time::Duration::from_secs(4);

        // Enable BNF SRU only for multi-word queries (≥3 raw words)
        // where niche French titles are likely missing from Inventaire/OpenLibrary.
        // Count raw words (not significant words) because French stop words like
        // "pour", "la" still indicate a specific multi-word title query
        // (e.g. "Agir pour la Guinée" = 4 raw words but only 2 significant).
        let raw_q = params
            .title
            .as_deref()
            .or(params.q.as_deref())
            .unwrap_or("");
        let raw_word_count = raw_q.split_whitespace().filter(|w| w.len() > 1).count();
        enable_bnf_sru = enable_bnf && raw_word_count >= 3;
    }

    // Apply source filter if provided (overrides profile settings)
    if let Some(ref filter) = params.source {
        let sources: Vec<&str> = filter.split(',').map(|s| s.trim()).collect();
        // When user explicitly selects sources, use ONLY those (truly override profile)
        enable_inventaire = sources.iter().any(|s| s.eq_ignore_ascii_case("inventaire"));
        let bnf_selected = sources
            .iter()
            .any(|s| s.eq_ignore_ascii_case("bnf") || s.eq_ignore_ascii_case("data.bnf.fr"));
        enable_bnf_sparql = bnf_selected;
        enable_bnf_sru = bnf_selected;
        enable_openlibrary = sources.iter().any(|s| {
            s.eq_ignore_ascii_case("openlibrary") || s.eq_ignore_ascii_case("open library")
        });
        enable_google_books = sources.iter().any(|s| {
            s.eq_ignore_ascii_case("google_books")
                || s.eq_ignore_ascii_case("google")
                || s.eq_ignore_ascii_case("googlebooks")
        });
    }

    // 1. Build Query String for Inventaire (General Search)
    let mut inv_query_parts = Vec::new();
    // Prioritize specific fields if available, but fallback to 'q'
    if let Some(t) = &params.title {
        inv_query_parts.push(t.clone());
    } else if let Some(q) = &params.q {
        // If no title, use q as part of title-like search for Inventaire
        inv_query_parts.push(q.clone());
    }

    if let Some(a) = &params.author {
        inv_query_parts.push(a.clone());
    }

    let inv_query = inv_query_parts.join(" ");
    // If constructed query is empty, try using raw 'q' as a fallback
    let final_inv_query = if inv_query.trim().is_empty() {
        params.q.clone().unwrap_or_default()
    } else {
        inv_query
    };

    // Extract primary user language for Inventaire (returns translated titles)
    let inv_lang = params
        .lang
        .as_deref()
        .and_then(|l| l.split(',').next())
        .map(|l| l.split('-').next().unwrap_or(l).trim().to_string())
        .filter(|l| !l.is_empty());

    // 3. Execute Searches in Parallel (Inventaire, BNF, OpenLibrary, BNF SRU)
    // We clone necessary data for each async task to avoid borrow checker issues with async blocks
    let inv_query_str = final_inv_query.clone();
    let bnf_query_str = final_inv_query.clone();
    let bnf_sru_query_str = final_inv_query.clone();
    let bnf_sru_title = params.title.clone();
    let bnf_sru_author = params.author.clone();
    let db_clone = db.clone();

    // Construct SearchQuery for search_external
    let search_query = crate::api::search::SearchQuery {
        q: params.q.clone(), // Use generic query explicitly
        title: params.title.clone(),
        author: params.author.clone(),
        publisher: params.publisher.clone(),
        year_min: None,
        year_max: None,
        tags: None,
        subjects: params.subject.clone(),
        sources: None,
        autocomplete: params.autocomplete,
    };
    // Clone search_query for the tasks
    let ol_query = search_query.clone();
    let gb_query = search_query.clone();

    // Determine if we should run OL search
    let run_ol = enable_openlibrary
        && (ol_query.q.is_some()
            || ol_query.title.is_some()
            || ol_query.author.is_some()
            || ol_query.publisher.is_some()
            || ol_query.subjects.is_some());

    // Determine if we should run Google Books search
    let run_gb = enable_google_books
        && (gb_query.q.is_some()
            || gb_query.title.is_some()
            || gb_query.author.is_some()
            || gb_query.publisher.is_some()
            || gb_query.subjects.is_some());

    // Execute ALL sources in parallel with individual error isolation
    // This ensures one slow/failing source doesn't block or crash others
    let (inv_res, ol_res, bnf_res, bnf_sru_res, gb_res) = tokio::join!(
        // Task 1: Inventaire (wrapped in timeout to prevent blocking)
        async move {
            if enable_inventaire && !inv_query_str.trim().is_empty() {
                // Use tokio timeout to prevent Inventaire from blocking indefinitely
                match tokio::time::timeout(std::time::Duration::from_secs(8), async {
                    match crate::inventaire_client::search_inventaire_with_lang(
                        &inv_query_str,
                        inv_lang.as_deref(),
                    )
                    .await
                    {
                        Ok(inv_results) => {
                            // Enrich results (also async)
                            match crate::inventaire_client::enrich_search_results(inv_results).await
                            {
                                Ok(res) => Ok(res),
                                Err(e) => Err(format!("Inventaire enrichment failed: {}", e)),
                            }
                        }
                        Err(e) => Err(format!("Inventaire search failed: {}", e)),
                    }
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => Ok(Vec::new()),
                }
            } else {
                Ok(Vec::new())
            }
        },
        // Task 2: OpenLibrary (wrapped in timeout to prevent blocking)
        async move {
            if run_ol {
                tokio::time::timeout(search_timeout, search_external(&ol_query, &db_clone))
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        },
        // Task 3: BNF SPARQL (wrapped in timeout for extra safety)
        async move {
            if enable_bnf_sparql && !bnf_query_str.trim().is_empty() {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    crate::modules::integrations::bnf::search_bnf(&bnf_query_str),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Ok(Vec::new()),
                }
            } else {
                Ok(Vec::new())
            }
        },
        // Task 4: BNF SRU (catalogue.bnf.fr - better coverage for recent French books)
        async move {
            if enable_bnf_sru
                && (!bnf_sru_query_str.trim().is_empty()
                    || bnf_sru_title.is_some()
                    || bnf_sru_author.is_some())
            {
                // In autocomplete, use tighter timeout and pass query as title hint
                // so SRU searches bib.title (indexed) instead of bib.anywhere (full-text)
                let (sru_timeout, sru_title) = if is_autocomplete {
                    let title_hint = bnf_sru_title
                        .as_deref()
                        .or(Some(bnf_sru_query_str.as_str()))
                        .filter(|s| !s.trim().is_empty());
                    (std::time::Duration::from_secs(4), title_hint)
                } else {
                    (std::time::Duration::from_secs(8), bnf_sru_title.as_deref())
                };
                match tokio::time::timeout(
                    sru_timeout,
                    crate::modules::integrations::bnf::search_bnf_sru(
                        &bnf_sru_query_str,
                        sru_title,
                        bnf_sru_author.as_deref(),
                    ),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Ok(Vec::new()),
                }
            } else {
                Ok(Vec::new())
            }
        },
        // Task 5: Google Books (wrapped in timeout)
        async move {
            if run_gb {
                tokio::time::timeout(
                    search_timeout,
                    crate::google_books::search_books(&gb_query, google_books_api_key.as_deref()),
                )
                .await
                .unwrap_or_default()
            } else {
                Vec::new()
            }
        }
    );

    // 4. Process Results

    // Process Inventaire Results
    if let Ok(ref enriched) = inv_res {
        for item in enriched {
            let authors = item.authors.clone();
            let author_name = authors.as_ref().map(|a| a.join(", "));

            let book = book::Book {
                id: None,
                title: item.label.clone(),
                isbn: item.isbn.clone(), // Now populated by enrichment
                publisher: item.publisher.clone(), // Resolved from Wikidata URI
                publication_year: None,
                summary: item.description.clone(),
                dewey_decimal: None,
                lcc: None,
                subjects: None,
                marc_record: None,
                cataloguing_notes: None,
                source_data: Some(
                    serde_json::json!({
                        "source": "inventaire",
                        "uri": item.uri,
                        "image_url": item.image
                    })
                    .to_string(),
                ),
                shelf_position: None,
                reading_status: Some("to_read".to_string()),
                source: Some("Inventaire".to_string()),
                author: author_name,
                authors,
                cover_url: item.image.clone(),
                large_cover_url: None,
                finished_reading_at: None,
                started_reading_at: None,
                user_rating: None,
                owned: Some(true),
                price: None,
                language: item.language.clone(), // Language from Wikidata
                digital_formats: None,
                available_copies: None,
            };
            results.push(book);
        }
    } else if let Err(_e) = inv_res {
    }

    // Process BNF Results
    match bnf_res {
        Ok(ref bnf_results) => {
            for bnf_book in bnf_results {
                let book = book::Book {
                    id: None,
                    title: bnf_book.title.clone(),
                    isbn: bnf_book.isbn.clone(),
                    publisher: bnf_book.publisher.clone(),
                    publication_year: bnf_book.publication_year,
                    summary: bnf_book.description.clone(),
                    dewey_decimal: None,
                    lcc: None,
                    subjects: None,
                    marc_record: None,
                    cataloguing_notes: None,
                    source_data: Some(
                        serde_json::json!({
                            "source": "bnf",
                            "bnf_uri": bnf_book.bnf_uri,
                            "languages": ["fr"]
                        })
                        .to_string(),
                    ),
                    shelf_position: None,
                    reading_status: Some("to_read".to_string()),
                    source: Some("BNF".to_string()),
                    author: bnf_book.author.clone(),
                    authors: bnf_book.author.clone().map(|a| vec![a]),
                    cover_url: bnf_book.cover_url.clone(),
                    large_cover_url: None,
                    finished_reading_at: None,
                    started_reading_at: None,
                    user_rating: None,
                    owned: Some(true),
                    price: None,
                    language: Some("fr".to_string()), // BNF is French National Library
                    digital_formats: None,
                    available_copies: None,
                };
                results.push(book);
            }
        }
        Err(_e) => (),
    }

    // Process BNF SRU Results (catalogue.bnf.fr)
    // SRU has richer metadata (ISBN, cover, publisher) than SPARQL, so when both
    // return the same book we prefer SRU and replace the SPARQL entry.
    match bnf_sru_res {
        Ok(ref bnf_sru_results) => {
            for bnf_book in bnf_sru_results {
                // Dedup: check ISBN match OR normalized title+author match
                let sru_title_norm = normalize_string(&bnf_book.title);
                let sru_author_norm = normalize_string(bnf_book.author.as_deref().unwrap_or(""));

                // Check if a duplicate already exists
                let dup_idx = results.iter().position(|b| {
                    // ISBN match
                    if let (Some(existing_isbn), Some(new_isbn)) = (&b.isbn, &bnf_book.isbn)
                        && existing_isbn == new_isbn
                    {
                        return true;
                    }
                    // Title+author match (normalized) — catches SPARQL entries without ISBN
                    let existing_title = normalize_string(&b.title);
                    let existing_author = normalize_string(b.author.as_deref().unwrap_or(""));
                    !sru_title_norm.is_empty()
                        && existing_title.contains(&sru_title_norm)
                        && (sru_author_norm.is_empty()
                            || existing_author.contains(&sru_author_norm)
                            || sru_author_norm.contains(&existing_author))
                });

                if let Some(idx) = dup_idx {
                    // SRU has better metadata — replace the existing entry only if
                    // the SRU result has strictly more data (ISBN or cover)
                    let existing = &results[idx];
                    let sru_has_more = (bnf_book.isbn.is_some() && existing.isbn.is_none())
                        || (bnf_book.cover_url.is_some() && existing.cover_url.is_none());
                    if !sru_has_more {
                        continue; // Existing entry is at least as good
                    }
                    results.remove(idx);
                    // Fall through to insert the SRU version below
                } else if let Some(ref isbn) = bnf_book.isbn
                    && results.iter().any(|b| b.isbn.as_ref() == Some(isbn))
                {
                    continue;
                }
                let book = book::Book {
                    id: None,
                    title: bnf_book.title.clone(),
                    isbn: bnf_book.isbn.clone(),
                    publisher: bnf_book.publisher.clone(),
                    publication_year: bnf_book.publication_year,
                    summary: bnf_book.description.clone(),
                    dewey_decimal: None,
                    lcc: None,
                    subjects: None,
                    marc_record: None,
                    cataloguing_notes: None,
                    source_data: Some(
                        serde_json::json!({
                            "source": "bnf-sru",
                            "bnf_uri": bnf_book.bnf_uri,
                            "languages": ["fr"]
                        })
                        .to_string(),
                    ),
                    shelf_position: None,
                    reading_status: Some("to_read".to_string()),
                    source: Some("BNF".to_string()),
                    author: bnf_book.author.clone(),
                    authors: bnf_book.author.clone().map(|a| vec![a]),
                    cover_url: bnf_book.cover_url.clone(),
                    large_cover_url: None,
                    finished_reading_at: None,
                    started_reading_at: None,
                    user_rating: None,
                    owned: Some(true),
                    price: None,
                    language: Some("fr".to_string()),
                    digital_formats: None,
                    available_copies: None,
                };
                results.push(book);
            }
        }
        Err(_e) => (),
    }

    // Process OpenLibrary Results - PARALLELIZED
    // Check time budget: skip expensive HTTP enrichment if search phase was slow.
    // Flutter client has a 15s receiveTimeout — leave at least 4s for enrichment.
    // Previous threshold of 7s was too tight: BNF SPARQL alone can take 8s,
    // causing enrichment skip and quality-filter removal of sparse OL results.
    let elapsed_after_search = search_start.elapsed();
    let skip_ol_enrichment = elapsed_after_search.as_secs() >= 11;
    if skip_ol_enrichment {
        tracing::debug!(
            "Skipping OL enrichment: search phase took {:.1}s (budget exceeded)",
            elapsed_after_search.as_secs_f64()
        );
    }

    let ol_processed_results: Vec<book::Book> = stream::iter(ol_res)
        .map(|model| {
            let skip_enrichment = skip_ol_enrichment;
            async move {
                // Convert Model to Book DTO and enrich
                let mut dto = book::Book::from(model.clone());

                // Extract author, cover, and language from source_data

                if let Some(source_data_str) = &model.source_data
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(source_data_str)
                {
                    if let Some(authors) = json.get("authors").and_then(|a| a.as_array()) {
                        let author_str = authors
                            .iter()
                            .map(|v| v.as_str().unwrap_or("").to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if !author_str.is_empty() {
                            dto.author = Some(author_str);
                        }
                    }
                    if let Some(cover_id) = json.get("cover_id").and_then(|v| v.as_i64()) {
                        dto.cover_url = Some(format!(
                            "https://covers.openlibrary.org/b/id/{}-L.jpg",
                            cover_id
                        ));
                    }
                    // Extract first language
                    if let Some(languages) = json.get("languages").and_then(|l| l.as_array())
                        && let Some(first_lang) = languages.first().and_then(|v| v.as_str())
                    {
                        dto.language = Some(first_lang.to_string());
                    }
                    // Extract publisher if missing
                    if dto.publisher.is_none() {
                        if let Some(publisher) = json.get("publisher").and_then(|p| p.as_array()) {
                            let pub_str = publisher
                                .iter()
                                .map(|v| v.as_str().unwrap_or("").to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            if !pub_str.is_empty() {
                                dto.publisher = Some(pub_str);
                            }
                        } else if let Some(publisher) =
                            json.get("publisher").and_then(|p| p.as_str())
                            && !publisher.is_empty()
                        {
                            dto.publisher = Some(publisher.to_string());
                        }
                    }
                    // Fallback: extract ISBN from source_data if model.isbn was None
                    if dto.isbn.is_none()
                        && let Some(isbns) = json.get("isbns").and_then(|i| i.as_array())
                        && let Some(first_isbn) = isbns.first().and_then(|v| v.as_str())
                    {
                        dto.isbn = Some(first_isbn.to_string());
                    }

                    let is_openlibrary = json
                        .get("source")
                        .and_then(|s| s.as_str())
                        .map(|s| s == "openlibrary")
                        .unwrap_or(true);

                    // Only do expensive HTTP enrichment if we have time budget remaining
                    if !skip_enrichment {
                        // Fallback: fetch from edition API if still no ISBN OR no publisher
                        if is_openlibrary
                            && (dto.isbn.is_none() || dto.publisher.is_none())
                            && let Some(edition_key) =
                                json.get("edition_key").and_then(|k| k.as_str())
                            && let Some((fetched_isbn, fetched_pub)) =
                                fetch_edition_extras(edition_key).await
                        {
                            if dto.isbn.is_none() {
                                dto.isbn = Some(fetched_isbn);
                            }
                            if dto.publisher.is_none() && fetched_pub.is_some() {
                                dto.publisher = fetched_pub;
                            }
                        }

                        // Extra Fallback: fetch from Work API if still no ISBN OR no publisher
                        if is_openlibrary
                            && (dto.isbn.is_none() || dto.publisher.is_none())
                            && let Some(work_key) = json.get("key").and_then(|k| k.as_str())
                            && let Some((fetched_isbn, fetched_pub)) =
                                fetch_work_edition_extras(work_key).await
                        {
                            if dto.isbn.is_none() {
                                dto.isbn = Some(fetched_isbn);
                            }
                            if dto.publisher.is_none() && fetched_pub.is_some() {
                                dto.publisher = fetched_pub;
                            }
                        }
                    }

                    // Set source based on the actual data
                    if let Some(source_str) = json.get("source").and_then(|s| s.as_str()) {
                        if source_str == "google_books" {
                            dto.source = Some("Google Books".to_string());
                        } else {
                            dto.source = Some("Open Library".to_string());
                        }
                    } else {
                        dto.source = Some("Open Library".to_string());
                    }
                }
                // dto.source is now set inside the block above based on data
                if dto.source.is_none() {
                    dto.source = Some("Open Library".to_string());
                }
                dto
            }
        })
        .buffer_unordered(10)
        .collect()
        .await;

    results.extend(ol_processed_results);

    // Process Google Books Results
    for model in gb_res {
        // Convert Model to Book DTO
        let mut dto = book::Book::from(model.clone());

        // Extract author, cover, and language from source_data
        if let Some(source_data_str) = &model.source_data
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(source_data_str)
        {
            // Extract author
            if let Some(authors) = json.get("authors").and_then(|a| a.as_array()) {
                let author_str = authors
                    .iter()
                    .map(|v| v.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                if !author_str.is_empty() {
                    dto.author = Some(author_str);
                }
            }
            // Extract language
            if let Some(lang) = json.get("language").and_then(|l| l.as_str()) {
                dto.language = Some(lang.to_string());
            }
        }
        dto.source = Some("Google Books".to_string());
        results.push(dto);
    }

    // Global Quality Filter: discard results that are too sparse
    // Keep results if they have at least one significant piece of metadata.
    // BNF (French National Library) is an authoritative source - keep its results
    // even without cover/ISBN since it has high-quality curated metadata.
    results.retain(|book| {
        let has_isbn = book.isbn.as_ref().is_some_and(|s| !s.trim().is_empty());
        let has_cover = book
            .cover_url
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_publisher = book
            .publisher
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty());

        // BNF is authoritative French National Library - keep ALL its results
        // BNF has high-quality curated metadata (title is always present and accurate)
        let is_bnf = book
            .source
            .as_ref()
            .is_some_and(|s| s.eq_ignore_ascii_case("BNF"));

        // Keep the result if it has at least one significant piece of metadata
        // OR if it's from BNF (authoritative French national library source)
        has_isbn || has_cover || has_publisher || is_bnf
    });

    // Cross-source notoriety propagation: copy edition_count from OpenLibrary
    // results to BNF/Inventaire results that represent the same work.
    // Uses fuzzy title matching on significant words + author surname to handle
    // cross-language variants (e.g. "El túnel" -> "Le tunnel" by same author).
    {
        // Collect books that have edition_count (typically from OpenLibrary)
        struct NotorietyEntry {
            title_words: Vec<String>,
            author_surname: String,
            edition_count: i32,
        }
        let mut notoriety_sources: Vec<NotorietyEntry> = Vec::new();
        for book in &results {
            let ec = get_edition_count(book);
            if ec > 0 {
                notoriety_sources.push(NotorietyEntry {
                    title_words: significant_words(&normalize_string(&book.title)),
                    author_surname: normalize_string(book.author.as_deref().unwrap_or(""))
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .to_string(),
                    edition_count: ec,
                });
            }
        }
        // Propagate to books without edition_count via fuzzy matching
        for book in &mut results {
            if get_edition_count(book) == 0 && !notoriety_sources.is_empty() {
                let book_title_words = significant_words(&normalize_string(&book.title));
                let book_surname = normalize_string(book.author.as_deref().unwrap_or(""))
                    .split_whitespace()
                    .last()
                    .unwrap_or("")
                    .to_string();

                // Find best matching notoriety source
                let mut best_ec = 0;
                for src in &notoriety_sources {
                    // Author surname must match (exact or fuzzy)
                    let author_match = !src.author_surname.is_empty()
                        && !book_surname.is_empty()
                        && (src.author_surname == book_surname
                            || jaro_winkler(&src.author_surname, &book_surname) >= 0.88);
                    if !author_match {
                        continue;
                    }
                    // Title significant words must fuzzy-match
                    let title_match = !book_title_words.is_empty()
                        && !src.title_words.is_empty()
                        && book_title_words.iter().all(|bw| {
                            src.title_words
                                .iter()
                                .any(|sw| sw == bw || jaro_winkler(sw, bw) >= 0.88)
                        });
                    if title_match && src.edition_count > best_ec {
                        best_ec = src.edition_count;
                    }
                    // Fallback: if author matches and this is the ONLY notable work
                    // by this author in the results, propagate even without title match.
                    // Handles cross-language titles like "Words" -> "Les Mots" (Sartre)
                    if !title_match && best_ec == 0 && src.edition_count > 20 {
                        let same_author_count = notoriety_sources
                            .iter()
                            .filter(|s| {
                                s.author_surname == src.author_surname
                                    || jaro_winkler(&s.author_surname, &src.author_surname) >= 0.88
                            })
                            .count();
                        if same_author_count == 1 {
                            best_ec = src.edition_count;
                        }
                    }
                }
                if best_ec > 0
                    && let Some(ref sd) = book.source_data
                    && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(sd)
                {
                    json["edition_count"] = serde_json::json!(best_ec);
                    book.source_data = Some(json.to_string());
                }
            }
        }
    }

    let query_author = params.author.as_deref().unwrap_or("").to_lowercase();
    let query_title = params.title.as_deref().unwrap_or("").to_lowercase();
    let query_q = params.q.as_deref().unwrap_or("").to_lowercase();
    // Parse multi-language param: "fr,en,es" -> Vec<String>
    let user_langs: Vec<String> = params
        .lang
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    // Relevance filter: discard results whose title shares no significant word
    // with the query (after removing stop words). This eliminates noise from
    // sources like Inventaire that match on articles ("Le", "Les", "The").
    let raw_query = if !query_title.is_empty() {
        &query_title
    } else {
        &query_q
    };
    if !raw_query.is_empty() {
        let query_words = significant_words(raw_query);
        if !query_words.is_empty() {
            results.retain(|book| {
                let title_words = significant_words(&normalize_string(&book.title));
                // Keep if at least one significant word overlaps (exact or fuzzy)
                query_words.iter().any(|qw| {
                    title_words
                        .iter()
                        .any(|tw| tw == qw || jaro_winkler(tw, qw) >= 0.88)
                })
            });
        }
    }

    // 5. Compute relevance scores and sort
    let mut scored: Vec<(i32, book::Book)> = results
        .into_iter()
        .map(|b| {
            let score = calculate_relevance(&b, &query_author, &query_title, &query_q, &user_langs);
            (score, b)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));

    for (score, book) in &scored {
        tracing::debug!(
            "RELEVANCE: {} ({}): score={} editions={} lang={:?}",
            book.title,
            book.source.as_deref().unwrap_or("?"),
            score,
            get_edition_count(book),
            book.language
        );
    }

    // Filter out self-publishing platforms and serialize with relevance_score
    let results: Vec<serde_json::Value> = scored
        .into_iter()
        .filter(|(_, book)| {
            book.publisher
                .as_ref()
                .map(|p| {
                    let pl = p.to_lowercase();
                    !pl.contains("createspace")
                        && !pl.contains("independently published")
                        && !pl.contains("independent publishing platform")
                })
                .unwrap_or(true)
        })
        .filter_map(|(score, book)| {
            // Serialize the book and inject relevance_score for Flutter consumption
            let mut val = serde_json::to_value(&book).ok()?;
            if let Some(obj) = val.as_object_mut() {
                obj.insert("relevance_score".to_string(), json!(score));
            }
            Some(val)
        })
        .collect();

    (StatusCode::OK, Json(results)).into_response()
}

/// Extract edition_count from a book's source_data JSON (OpenLibrary popularity signal).
fn get_edition_count(book: &book::Book) -> i32 {
    book.source_data
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|json| json.get("edition_count")?.as_i64())
        .unwrap_or(0) as i32
}

/// Multilingual stop words set, built once from the `stop-words` crate.
/// Covers all app languages (fr, en, es, de, it, pt, tr, bg) and more.
static STOP_WORDS: LazyLock<std::collections::HashSet<String>> = LazyLock::new(|| {
    use stop_words::LANGUAGE;
    let langs = [
        LANGUAGE::French,
        LANGUAGE::English,
        LANGUAGE::Spanish,
        LANGUAGE::German,
        LANGUAGE::Italian,
        LANGUAGE::Portuguese,
        LANGUAGE::Turkish,
        LANGUAGE::Bulgarian,
        LANGUAGE::Dutch,
        LANGUAGE::Swedish,
        LANGUAGE::Norwegian,
        LANGUAGE::Danish,
        LANGUAGE::Romanian,
        LANGUAGE::Arabic,
        LANGUAGE::Japanese,
        LANGUAGE::Chinese,
    ];
    let mut set = std::collections::HashSet::new();
    for lang in langs {
        for w in stop_words::get(lang) {
            set.insert(w.to_lowercase());
        }
    }
    set
});

fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(word)
}

/// Extract significant (non-stop) words from a normalized string.
fn significant_words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.trim())
        .filter(|w| w.len() > 1 && !is_stop_word(w))
        .map(|w| w.to_string())
        .collect()
}

/// Best fuzzy similarity between a query phrase and a title, using a sliding
/// window of query-word-count over the title words. This handles titles like
/// "Jack London - Martin Eden" matching query "Martin Eden".
fn best_phrase_similarity(title: &str, query: &str) -> f64 {
    let t_words = significant_words(title);
    let q_words = significant_words(query);
    if q_words.is_empty() || t_words.is_empty() {
        return 0.0;
    }
    // Compare as full normalized phrases first
    let t_joined: String = t_words.join(" ");
    let q_joined: String = q_words.join(" ");
    let full_sim = jaro_winkler(&t_joined, &q_joined);

    // Also try sliding window of q_words.len() over t_words
    let window = q_words.len();
    let mut best = full_sim;
    if t_words.len() >= window {
        for start in 0..=(t_words.len() - window) {
            let chunk: String = t_words[start..start + window].join(" ");
            let sim = jaro_winkler(&chunk, &q_joined);
            if sim > best {
                best = sim;
            }
        }
    }
    best
}

fn calculate_relevance(
    book: &book::Book,
    q_author: &str,
    q_title: &str,
    q_any: &str,
    user_langs: &[String],
) -> i32 {
    let mut score = 0;

    let title = normalize_string(&book.title);
    let author = normalize_string(book.author.as_deref().unwrap_or(""));
    let q_author = normalize_string(q_author);
    let q_title = normalize_string(q_title);
    let q_any = normalize_string(q_any);

    // --- Notoriety: edition_count from OpenLibrary ---
    let edition_count = get_edition_count(book);
    // Logarithmic scale with tiered bonuses for established and classic works.
    // Base: 1 ed = 0, 10 ed ~ 46, 50 ed ~ 78, 100 ed ~ 92
    let notoriety_score = if edition_count > 1 {
        let mut n = ((edition_count as f64).ln() * 20.0).min(120.0) as i32;
        // Tiered bonus: established works (>20 ed) and classics (>80 ed)
        if edition_count > 80 {
            n += 100; // Classic: Sabato, London, Verne...
        } else if edition_count > 20 {
            n += 40; // Established work
        }
        n
    } else {
        0
    };
    score += notoriety_score;

    // --- Language Match ---
    // When reading languages are configured, book language is a strong relevance signal.
    // A matching language gets a significant boost; non-matching gets a penalty
    // (reduced for widely-published classics so they still appear, just lower).
    if !user_langs.is_empty() {
        if let Some(ref lang) = book.language {
            if lang_matches_any(lang, user_langs) {
                score += 80;
            } else {
                // Penalty for non-matching language, reduced for widely-published books
                let penalty = if edition_count > 20 { -15 } else { -50 };
                score += penalty;
            }
        } else {
            // No language info: mild penalty (unknown language can't confirm match)
            score -= 20;
        }

        // Check source_data for additional language signals
        if let Some(source_data_str) = &book.source_data
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(source_data_str)
            && let Some(languages) = json.get("languages").and_then(|l| l.as_array())
        {
            let mut found_match = false;
            for lang in languages {
                if let Some(lang_str) = lang.as_str()
                    && lang_matches_any(lang_str, user_langs)
                {
                    score += 40;
                    found_match = true;
                    break;
                }
            }
            if !found_match && !languages.is_empty() {
                let penalty = if edition_count > 20 { -10 } else { -30 };
                score += penalty;
            }
        }
    }

    // Source boost for French users: BNF (national library)
    // Minor signal - metadata quality, not relevance
    if user_langs.iter().any(|l| {
        let b = base_lang(l);
        b == "fr" || b == "fra" || b == "fre"
    }) && let Some(source) = &book.source
        && source == "BNF"
    {
        score += 20;
    }

    // --- Author Match (exact > contains > fuzzy) ---
    if !q_author.is_empty() {
        if author == q_author {
            score += 100;
        } else if author.contains(&q_author) {
            score += 50;
        } else {
            let sim = jaro_winkler(&author, &q_author);
            if sim >= 0.88 {
                score += (sim * 40.0) as i32;
            }
        }
    }

    // --- Title Match (exact > contains > fuzzy phrase) ---
    // Title match is the strongest signal - must dominate over metadata bonuses.
    // For short queries like "Les mots", many titles contain those words.
    // Scale the contains bonus by coverage ratio so exact/near-exact titles win.
    if !q_title.is_empty() {
        if title == q_title {
            score += 200;
        } else if title.contains(&q_title) {
            // Coverage: how much of the title does the query cover?
            // "les mots" in "les mots et les choses" = 8/25 = 0.32 -> low bonus
            // "les mots" in "les mots perdus" = 8/15 = 0.53 -> medium bonus
            let coverage = q_title.len() as f64 / title.len() as f64;
            // Scale from 40 (low coverage) to 150 (high coverage, near-exact)
            score += (40.0 + coverage * 110.0) as i32;
        } else {
            let sim = best_phrase_similarity(&title, &q_title);
            if sim >= 0.95 {
                // Near-identical: accent/spelling/language variant
                // "el tunel" vs "le tunnel" (sim ~0.96) should score almost like exact
                score += 180;
            } else if sim >= 0.88 {
                // Good fuzzy match
                score += (sim * 100.0) as i32;
            }
        }
    }

    // --- General Query Match (exact > contains > fuzzy) ---
    if !q_any.is_empty() {
        // Author
        if author.contains(&q_any) {
            score += 30;
        } else {
            let sim = jaro_winkler(&author, &q_any);
            if sim >= 0.88 {
                score += (sim * 20.0) as i32;
            }
        }
        // Title
        if title.contains(&q_any) {
            score += 30;
        } else {
            let sim = best_phrase_similarity(&title, &q_any);
            if sim >= 0.88 {
                score += (sim * 25.0) as i32;
            }
        }
    }

    // --- Word Coverage Bonus (multi-word queries, ≥3 significant words) ---
    // When the user types a long, specific query, the proportion of query words
    // found in the title is the strongest relevance signal — stronger than notoriety.
    // Uses quadratic scaling so full coverage (4/4) massively outscores partial (1/4).
    let query_for_coverage = if !q_title.is_empty() {
        &q_title
    } else if !q_any.is_empty() {
        &q_any
    } else {
        ""
    };
    if !query_for_coverage.is_empty() {
        let q_words = significant_words(query_for_coverage);
        if q_words.len() >= 3 {
            let title_words = significant_words(&title);
            let matched = q_words
                .iter()
                .filter(|qw| {
                    title_words
                        .iter()
                        .any(|tw| tw == *qw || jaro_winkler(tw, qw) >= 0.88)
                })
                .count();
            let ratio = matched as f64 / q_words.len() as f64;
            score += (ratio * ratio * 300.0) as i32;
        }
    }

    // --- Metadata completeness ---
    let has_publisher = book
        .publisher
        .as_ref()
        .is_some_and(|p| !p.trim().is_empty());
    let has_cover = book
        .cover_url
        .as_ref()
        .is_some_and(|c| !c.trim().is_empty());

    if has_cover && has_publisher {
        score += 20;
    }

    if has_publisher {
        score += 10;
    }

    // Boost common publishers (minor signal, should not override title relevance)
    if let Some(publisher) = &book.publisher {
        let common_publishers = [
            "Livre de Poche",
            "Pocket",
            "Folio",
            "Gallimard",
            "J'ai lu",
            "Flammarion",
            "Seuil",
            "Points",
            "10/18",
        ];
        for common in common_publishers {
            if publisher.to_lowercase().contains(&common.to_lowercase()) {
                score += 15;
                break;
            }
        }

        // Penalty for self-published
        let publisher_lower = publisher.to_lowercase();
        if publisher_lower.contains("independently published")
            || publisher_lower.contains("createspace")
        {
            score -= 100;
        }
    }

    if has_cover {
        score += 10;
    }

    if book.summary.is_some() {
        score += 5;
    }

    score
}

/// MCP Configuration endpoint for AI Assistant integrations (Claude Desktop, Cursor, Continue, etc.)
/// Returns a ready-to-use JSON configuration with dynamic paths
pub async fn mcp_config() -> impl IntoResponse {
    // Get the current executable path
    // In development (Flutter Debug), current_exe points to the App Bundle.
    // We check the standard development location first using an absolute path
    // to avoid Sandbox issues with $HOME.
    let dev_path = std::path::PathBuf::from(
        "/Users/federico/Sites/bibliotech/bibliogenius/target/debug/bibliogenius",
    );

    let binary_path = if dev_path.exists() {
        dev_path.to_string_lossy().to_string()
    } else {
        std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/path/to/bibliogenius".to_string())
    };

    // Get the database URL from environment
    // Default to Application Support directory (same as Flutter app)
    let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        #[cfg(target_os = "macos")]
        {
            if let Ok(home) = std::env::var("HOME") {
                // macOS: Flutter uses getApplicationSupportDirectory() → ~/Library/Application Support/
                format!("sqlite://{}/Library/Application Support/com.bibliogenius.app/bibliogenius.db?mode=rwc", home)
            } else {
                "sqlite:///path/to/bibliogenius.db?mode=rwc".to_string()
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
                format!("sqlite://{}/BiblioGenius/bibliogenius.db?mode=rwc", appdata)
            } else {
                "sqlite:///path/to/bibliogenius.db?mode=rwc".to_string()
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(home) = std::env::var("HOME") {
                format!(
                    "sqlite://{}/.local/share/bibliogenius/bibliogenius.db?mode=rwc",
                    home
                )
            } else {
                "sqlite:///path/to/bibliogenius.db?mode=rwc".to_string()
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            "sqlite:///path/to/bibliogenius.db?mode=rwc".to_string()
        }
    });

    let config = json!({
        "mcpServers": {
            "bibliogenius": {
                "command": binary_path,
                "args": ["--mcp"],
                "env": {
                    "DATABASE_URL": database_url
                }
            }
        }
    });

    (StatusCode::OK, Json(json!({
        "config": config,
        "config_json": serde_json::to_string_pretty(&config).unwrap_or_default(),
        "compatible_clients": [
            "Claude Desktop",
            "Cursor",
            "Continue.dev",
            "Cline (VS Code)",
            "Zed Editor",
            "Sourcegraph Cody"
        ],
        "instructions": "Paste this configuration into your AI assistant's MCP configuration file (e.g., claude_desktop_config.json for Claude Desktop)."
    }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_matches_strips_regional_codes() {
        // pt-BR user should match pt books
        assert!(lang_matches("pt", "pt-BR"));
        assert!(lang_matches("pt-BR", "pt"));
        assert!(lang_matches("pt-BR", "pt-PT"));
        assert!(lang_matches("pt-PT", "pt-BR"));
    }

    #[test]
    fn lang_matches_chinese_variants() {
        // zh-CN and zh-TW should match zh and each other
        assert!(lang_matches("zh", "zh-CN"));
        assert!(lang_matches("zh-CN", "zh"));
        assert!(lang_matches("zh-TW", "zh"));
        assert!(lang_matches("zh", "zh-TW"));
        assert!(lang_matches("zh-CN", "zh-TW"));
    }

    #[test]
    fn lang_matches_iso639_2_with_regional() {
        // 3-letter codes should still work with regional variants
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
        let user_langs = vec!["pt-br".to_string(), "en".to_string()];
        assert!(lang_matches_any("pt", &user_langs));
        assert!(lang_matches_any("pt-PT", &user_langs));
        assert!(lang_matches_any("eng", &user_langs));
        assert!(!lang_matches_any("fr", &user_langs));
    }
}
