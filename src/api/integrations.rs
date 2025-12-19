use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use serde_json::json;

use crate::models::book;
use crate::modules::integrations::sudoc;

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

#[derive(Deserialize)]
pub struct OsmSearchQuery {
    lat: f64,
    lon: f64,
    radius: Option<u32>,
}

pub async fn search_osm_libraries(Query(params): Query<OsmSearchQuery>) -> impl IntoResponse {
    let radius = params.radius.unwrap_or(5000); // Default 5km
    match crate::modules::integrations::osm::find_nearby_libraries(params.lat, params.lon, radius)
        .await
    {
        Ok(nodes) => (StatusCode::OK, Json(nodes)).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn search_osm_bookstores(Query(params): Query<OsmSearchQuery>) -> impl IntoResponse {
    let radius = params.radius.unwrap_or(5000); // Default 5km
    match crate::modules::integrations::osm::find_nearby_bookstores(params.lat, params.lon, radius)
        .await
    {
        Ok(nodes) => (StatusCode::OK, Json(nodes)).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
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
    publisher: Option<Vec<String>>,
    isbn: Option<Vec<String>>,
    cover_i: Option<i32>,
    language: Option<Vec<String>>,
}

pub async fn search_external(query: &crate::api::search::SearchQuery) -> Vec<book::Model> {
    let mut books = Vec::new();
    let client = reqwest::Client::new();

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
    // If using generic q, the params are already encoded and formatted
    let url = if query.q.is_some() {
        format!("https://openlibrary.org/search.json?{}&limit=5", q_str)
    } else {
        // Fallback for specific fields (legacy construction)
        let q_str_legacy = q_parts.join(" AND ");
        format!(
            "https://openlibrary.org/search.json?q={}&limit=5",
            urlencoding::encode(&q_str_legacy)
        )
    };

    if let Ok(res) = client.get(&url).send().await {
        if let Ok(data) = res.json::<OpenLibrarySearchResponse>().await {
            for doc in data.docs {
                let isbn = doc.isbn.as_ref().and_then(|v| v.first()).cloned();

                // Map to our Book Model (store additional data in source_data)
                let source_data = serde_json::json!({
                    "authors": doc.author_name.clone().unwrap_or_default(),
                    "cover_id": doc.cover_i,
                    "source": "openlibrary",
                    "languages": doc.language.clone().unwrap_or_default()
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
                };
                books.push(book);
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
}

pub async fn search_unified(Query(params): Query<UnifiedSearchQuery>) -> impl IntoResponse {
    let mut results: Vec<book::Book> = Vec::new();

    // 1. Build Query String for Inventaire (General Search)
    let mut inv_query_parts = Vec::new();
    if let Some(q) = &params.q {
        inv_query_parts.push(q.clone());
    }
    if let Some(t) = &params.title {
        inv_query_parts.push(t.clone());
    }
    if let Some(a) = &params.author {
        inv_query_parts.push(a.clone());
    }

    let inv_query = inv_query_parts.join(" ");

    // 2. Try Inventaire if we have a query string
    if !inv_query.trim().is_empty() {
        if let Ok(inv_results) = crate::inventaire_client::search_inventaire(&inv_query).await {
            // Enrich results with author names
            let enriched = match crate::inventaire_client::enrich_search_results(inv_results).await
            {
                Ok(res) => res,
                Err(e) => {
                    eprintln!("Inventaire enrichment failed: {}", e);
                    Vec::new()
                }
            };

            for item in enriched {
                let author_name = item.authors.as_ref().map(|a| a.join(", "));

                let book = book::Book {
                    id: None,
                    title: item.label,
                    isbn: None,
                    publisher: None,
                    publication_year: None,
                    summary: item.description,
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
                    cover_url: item.image,
                    large_cover_url: None,
                    finished_reading_at: None,
                    started_reading_at: None,
                    user_rating: None,
                    owned: Some(true), // Search results default to owned
                };
                results.push(book);
            }
        }
    }

    // 3. Always Search OpenLibrary (via search_external) for better coverage
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
    };

    // Only call if we have something to search
    if search_query.q.is_some()
        || search_query.title.is_some()
        || search_query.author.is_some()
        || search_query.publisher.is_some()
        || search_query.subjects.is_some()
    {
        let ol_results = search_external(&search_query).await;
        for model in ol_results {
            // Convert Model to Book DTO and enrich
            let mut dto = book::Book::from(model.clone());

            // Extract author and cover from source_data if present (search_external puts them there)
            if let Some(source_data_str) = &model.source_data {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(source_data_str) {
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
                }
            }
            dto.source = Some("Open Library".to_string());
            results.push(dto);
        }
    }

    // 4. Sort Results by Relevance
    // Prioritize:
    // 1. Language matches user preference
    // 2. Author matches query author (if provided)
    // 3. Title matches query title (if provided)
    // 4. Author matches general query 'q'

    let query_author = params.author.as_deref().unwrap_or("").to_lowercase();
    let query_title = params.title.as_deref().unwrap_or("").to_lowercase();
    let query_q = params.q.as_deref().unwrap_or("").to_lowercase();
    let user_lang = params.lang.as_deref().unwrap_or("").to_lowercase();

    results.sort_by(|a, b| {
        let score_a = calculate_relevance(a, &query_author, &query_title, &query_q, &user_lang);
        let score_b = calculate_relevance(b, &query_author, &query_title, &query_q, &user_lang);
        score_b.cmp(&score_a) // Descending score
    });

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

    let title = book.title.to_lowercase();
    let author = book.author.as_deref().unwrap_or("").to_lowercase();

    // Language Match - highest priority for user experience
    if !user_lang.is_empty() {
        if let Some(source_data_str) = &book.source_data {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(source_data_str) {
                if let Some(languages) = json.get("languages").and_then(|l| l.as_array()) {
                    for lang in languages {
                        if let Some(lang_str) = lang.as_str() {
                            // Check for match (e.g., "fre" matches "fr", "fra", "french")
                            if lang_str.to_lowercase().starts_with(user_lang)
                                || user_lang.starts_with(&lang_str.to_lowercase())
                            {
                                score += 40; // Strong language preference boost
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Author Match
    if !q_author.is_empty() {
        if author == q_author {
            score += 100;
        } else if author.contains(q_author) {
            score += 50;
        }
    }

    // Title Match
    if !q_title.is_empty() {
        if title == q_title {
            score += 80;
        } else if title.contains(q_title) {
            score += 40;
        }
    }

    // General Query Match
    if !q_any.is_empty() {
        if author.contains(q_any) {
            score += 30;
        }
        if title.contains(q_any) {
            score += 30;
        }
    }

    // Boost items with covers
    if book.cover_url.is_some() {
        score += 5;
    }

    // Boost items with summaries
    if book.summary.is_some() {
        score += 5;
    }

    score
}
