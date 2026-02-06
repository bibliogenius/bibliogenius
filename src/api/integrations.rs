use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use serde_json::json;

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
    key: String,                      // Work ID (e.g. "/works/OL12345W")
}

// Helper to check if language matches (handles 2-letter vs 3-letter codes)
fn lang_matches(book_lang: &str, user_lang: &str) -> bool {
    if user_lang.is_empty() {
        return true;
    }
    let b = book_lang.to_lowercase();
    let u = user_lang.to_lowercase();

    if b == u {
        return true;
    }

    // Simple mapping for common languages
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
    )
}

fn normalize_string(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'à' | 'â' | 'ä' => 'a',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'û' | 'ù' | 'ü' => 'u',
            'ç' => 'c',
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
        let mut q_parts = Vec::new();

        // If we have a generic query 'q', use it directly
        if let Some(q) = &query.q {
            q_parts.push(format!("q={}", urlencoding::encode(q)));
        } else {
            // Otherwise use specific fields
            if let Some(t) = &query.title {
                q_parts.push(format!("title:{}", t));
            }
            if let Some(a) = &query.author {
                q_parts.push(format!("author:{}", a));
            }
            if let Some(s) = &query.subjects {
                q_parts.push(format!("subject:{}", s));
            }
        }

        if q_parts.is_empty() {
            return books;
        }

        let q_str = q_parts.join("&");
        let limit_val = if query.autocomplete.unwrap_or(false) {
            12 // More results for autocomplete to allow quality filtering
        } else {
            20
        };
        let url = if query.q.is_some() {
            format!(
                "https://openlibrary.org/search.json?{}&limit={}",
                q_str, limit_val
            )
        } else {
            // Fallback for specific fields (legacy construction)
            let q_str_legacy = q_parts.join(" AND ");
            format!(
                "https://openlibrary.org/search.json?q={}&limit={}",
                urlencoding::encode(&q_str_legacy),
                limit_val
            )
        };

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

    // Load profile config to check enabled providers
    let (mut enable_inventaire, mut enable_bnf, mut enable_openlibrary, mut enable_google_books) =
        match ProfileEntity::find_by_id(1).one(&db).await {
            Ok(Some(profile_model)) => {
                let modules: Vec<String> =
                    serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
                let inv = !modules.contains(&"disable_fallback:inventaire".to_string());
                let bnf = !modules.contains(&"disable_fallback:bnf".to_string());
                let ol = !modules.contains(&"disable_fallback:openlibrary".to_string());
                let gb = modules.contains(&"enable_google_books".to_string());
                (inv, bnf, ol, gb)
            }
            _ => (true, true, true, false),
        };

    let is_autocomplete = params.autocomplete.unwrap_or(false);
    let mut search_timeout = std::time::Duration::from_secs(8);

    if is_autocomplete {
        // In autocomplete mode, disable only slow sources (BNF SPARQL)
        // Keep Inventaire enabled - it has good metadata (covers, publishers)
        enable_bnf = false;
        search_timeout = std::time::Duration::from_secs(4);
    }

    // Apply source filter if provided (overrides profile settings)
    if let Some(ref filter) = params.source {
        let sources: Vec<&str> = filter.split(',').map(|s| s.trim()).collect();
        // When user explicitly selects sources, use ONLY those (truly override profile)
        enable_inventaire = sources.iter().any(|s| s.eq_ignore_ascii_case("inventaire"));
        enable_bnf = sources
            .iter()
            .any(|s| s.eq_ignore_ascii_case("bnf") || s.eq_ignore_ascii_case("data.bnf.fr"));
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
                    match crate::inventaire_client::search_inventaire(&inv_query_str).await {
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
        // Task 3: BNF (wrapped in timeout for extra safety)
        async move {
            if enable_bnf && !bnf_query_str.trim().is_empty() {
                // Use tokio timeout to prevent BNF from blocking indefinitely
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
            if enable_bnf
                && (!bnf_sru_query_str.trim().is_empty()
                    || bnf_sru_title.is_some()
                    || bnf_sru_author.is_some())
            {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    crate::modules::integrations::bnf::search_bnf_sru(
                        &bnf_sru_query_str,
                        bnf_sru_title.as_deref(),
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
                tokio::time::timeout(search_timeout, crate::google_books::search_books(&gb_query))
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
                };
                results.push(book);
            }
        }
        Err(_e) => (),
    }

    // Process OpenLibrary Results - PARALLELIZED
    // Check time budget: skip expensive HTTP enrichment if search phase was slow
    // Flutter client has a 15s receiveTimeout, so we must stay well under that
    let elapsed_after_search = search_start.elapsed();
    let skip_ol_enrichment = elapsed_after_search.as_secs() >= 7;
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

    let query_author = params.author.as_deref().unwrap_or("").to_lowercase();
    let query_title = params.title.as_deref().unwrap_or("").to_lowercase();
    let query_q = params.q.as_deref().unwrap_or("").to_lowercase();
    let user_lang = params.lang.as_deref().unwrap_or("").to_lowercase();

    // 4. Language handling: ORDER by preferred language instead of filtering
    // This keeps all results but puts matching languages first
    // The ordering is handled by calculate_relevance() which gives +100 points
    // for language matches and -100 penalty for mismatches.
    // No filtering here - all results are kept for maximum coverage.

    // 5. Sort Results by Relevance
    // Prioritize:
    // 1. Language matches user preference
    // 2. Title matches query title (if provided)
    // 3. Author matches general query 'q'
    results.sort_by(|a, b| {
        let score_a = calculate_relevance(a, &query_author, &query_title, &query_q, &user_lang);
        let score_b = calculate_relevance(b, &query_author, &query_title, &query_q, &user_lang);

        // Descending Match: highest scores first
        score_b.cmp(&score_a)
    });

    for book in &results {
        let score = calculate_relevance(book, &query_author, &query_title, &query_q, &user_lang);
        tracing::debug!(
            "Final Result: {} ({}): Score {}",
            book.title,
            book.publisher.as_deref().unwrap_or("N/A"),
            score
        );
    }

    // Filter out self-publishing platforms (low-quality results)
    let results: Vec<_> = results
        .into_iter()
        .filter(|book| {
            if let Some(ref publisher) = book.publisher {
                let pub_lower = publisher.to_lowercase();
                !pub_lower.contains("createspace")
                    && !pub_lower.contains("independently published")
                    && !pub_lower.contains("independent publishing platform")
            } else {
                true // Keep books without publisher info
            }
        })
        .collect();

    (StatusCode::OK, Json(results)).into_response()
}

fn calculate_relevance(
    book: &book::Book,
    q_author: &str,
    q_title: &str,
    q_any: &str,
    user_lang: &str,
) -> i32 {
    let mut score = 0;

    let title = normalize_string(&book.title);
    let author = normalize_string(book.author.as_deref().unwrap_or(""));
    let q_author = normalize_string(q_author);
    let q_title = normalize_string(q_title);
    let q_any = normalize_string(q_any);

    // Language Match - highest priority for user experience
    if !user_lang.is_empty() {
        // 1. Check direct language field
        if let Some(ref lang) = book.language {
            if lang_matches(lang, user_lang) {
                score += 100; // Even stronger boost for direct language match
            } else {
                score -= 100; // PENALTY for explicit mismatch
            }
        }

        // 2. Check source_data for languages
        if let Some(source_data_str) = &book.source_data
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(source_data_str)
            && let Some(languages) = json.get("languages").and_then(|l| l.as_array())
        {
            let mut found_match = false;
            for lang in languages {
                if let Some(lang_str) = lang.as_str()
                    && lang_matches(lang_str, user_lang)
                {
                    score += 60;
                    found_match = true;
                    break;
                }
            }
            if !found_match && !languages.is_empty() {
                score -= 50; // Mismatch penalty in source data
            }
        }
    }

    // Source boost for French users: Prioritize BNF (national library) for French content
    if (user_lang == "fr" || user_lang == "fra" || user_lang == "fre")
        && let Some(source) = &book.source
        && source == "BNF"
    {
        score += 50; // National library bonus for French users
    }

    // Author Match
    if !q_author.is_empty() {
        if author == q_author {
            score += 100;
        } else if author.contains(&q_author) {
            score += 50;
        }
    }

    // Title Match
    if !q_title.is_empty() {
        if title == q_title {
            score += 80;
        } else if title.contains(&q_title) {
            score += 40;
        }
    }

    // General Query Match
    if !q_any.is_empty() {
        if author.contains(&q_any) {
            score += 30;
        }
        if title.contains(&q_any) {
            score += 30;
        }
    }

    // Check metadata completeness
    let has_publisher = book
        .publisher
        .as_ref()
        .map(|p| !p.trim().is_empty())
        .unwrap_or(false);
    let has_cover = book
        .cover_url
        .as_ref()
        .map(|c| !c.trim().is_empty())
        .unwrap_or(false);

    // Strong bonus for having BOTH cover AND publisher (complete metadata)
    if has_cover && has_publisher {
        score += 50; // Significant boost for complete results
    }

    // Boost for any publisher present
    if has_publisher {
        score += 15;
    }

    // Boost for common publishers (e.g. Livre de Poche, Pocket, etc.)
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
                score += 30; // Stronger boost for common editions
                break;
            }
        }

        // Penalty for "Independently Published" or "CreateSpace"
        let publisher_lower = publisher.to_lowercase();
        if publisher_lower.contains("independently published")
            || publisher_lower.contains("createspace")
        {
            score -= 100; // Heavy penalty for self-published/low-quality metadata editions
        }
    }

    // Boost items with covers
    if has_cover {
        score += 25; // Higher boost for visual results
    }

    // Boost items with summaries
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
