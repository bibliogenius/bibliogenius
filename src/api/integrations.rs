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

pub async fn search_external(
    query: &crate::api::search::SearchQuery,
    db: &DatabaseConnection,
) -> Vec<book::Model> {
    // Check if OpenLibrary fallback is enabled
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    let (enable_openlibrary, enable_google) =
        if let Ok(Some(profile_model)) = ProfileEntity::find_by_id(1).one(db).await {
            let modules: Vec<String> =
                serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
            (
                !modules.contains(&"disable_fallback:openlibrary".to_string()),
                modules.contains(&"enable_google_books".to_string()),
            )
        } else {
            (true, false)
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
                        price: None, // No price from external search
                    };
                    books.push(book);
                }
            }
        }
    }

    if enable_google {
        let gb_results = crate::google_books::search_books(query).await;
        books.extend(gb_results);
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

pub async fn search_unified(
    State(db): State<DatabaseConnection>,
    Query(params): Query<UnifiedSearchQuery>,
) -> impl IntoResponse {
    let mut results: Vec<book::Book> = Vec::new();

    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;
    // Load profile config to check enabled providers
    let (enable_inventaire, enable_bnf, enable_openlibrary) =
        if let Ok(Some(profile_model)) = ProfileEntity::find_by_id(1).one(&db).await {
            let modules: Vec<String> =
                serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
            (
                !modules.contains(&"disable_fallback:inventaire".to_string()),
                !modules.contains(&"disable_fallback:bnf".to_string()),
                !modules.contains(&"disable_fallback:openlibrary".to_string()),
            )
        } else {
            println!("DEBUG SEARCH: Profile not found");
            (true, true, true)
        };

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

    // 3. Execute Searches in Parallel (Inventaire, BNF, OpenLibrary)
    // We clone necessary data for each async task to avoid borrow checker issues with async blocks
    let inv_query_str = final_inv_query.clone();
    let bnf_query_str = final_inv_query.clone();
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
    };
    // Clone search_query for the task
    let ol_query = search_query.clone();

    // Determine if we should run OL search
    let run_ol = enable_openlibrary
        && (ol_query.q.is_some()
            || ol_query.title.is_some()
            || ol_query.author.is_some()
            || ol_query.publisher.is_some()
            || ol_query.subjects.is_some());

    let (inv_res, bnf_res, ol_res) = tokio::join!(
        // Task 1: Inventaire
        async move {
            if enable_inventaire && !inv_query_str.trim().is_empty() {
                match crate::inventaire_client::search_inventaire(&inv_query_str).await {
                    Ok(inv_results) => {
                        // Enrich results (also async)
                        match crate::inventaire_client::enrich_search_results(inv_results).await {
                            Ok(res) => Ok(res),
                            Err(e) => Err(format!("Inventaire enrichment failed: {}", e)),
                        }
                    }
                    Err(e) => Err(format!("Inventaire search failed: {}", e)),
                }
            } else {
                Ok(Vec::new())
            }
        },
        // Task 2: BNF
        async move {
            if enable_bnf && !bnf_query_str.trim().is_empty() {
                crate::modules::integrations::bnf::search_bnf(&bnf_query_str).await
            } else {
                Ok(Vec::new()) // Return empty vec if disabled
            }
        },
        // Task 3: OpenLibrary
        async move {
            if run_ol {
                search_external(&ol_query, &db_clone).await
            } else {
                Vec::new()
            }
        }
    );

    // 4. Process Results

    // Process Inventaire Results
    if let Ok(enriched) = inv_res {
        for item in enriched {
            let authors = item.authors.clone();
            let author_name = authors.as_ref().map(|a| a.join(", "));

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
                authors,
                cover_url: item.image,
                large_cover_url: None,
                finished_reading_at: None,
                started_reading_at: None,
                user_rating: None,
                owned: Some(true),
                price: None,
            };
            results.push(book);
        }
    } else if let Err(e) = inv_res {
        eprintln!("DEBUG SEARCH: {}", e);
    }

    // Process BNF Results
    match bnf_res {
        Ok(bnf_results) => {
            for bnf_book in bnf_results {
                let book = book::Book {
                    id: None,
                    title: bnf_book.title,
                    isbn: bnf_book.isbn,
                    publisher: bnf_book.publisher,
                    publication_year: bnf_book.publication_year,
                    summary: bnf_book.description,
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
                    authors: bnf_book.author.map(|a| vec![a]),
                    cover_url: bnf_book.cover_url,
                    large_cover_url: None,
                    finished_reading_at: None,
                    started_reading_at: None,
                    user_rating: None,
                    owned: Some(true),
                    price: None,
                };
                results.push(book);
            }
        }
        Err(e) => eprintln!("BNF search error: {}", e),
    }

    // Process OpenLibrary Results
    for model in ol_res {
        // Convert Model to Book DTO and enrich
        let mut dto = book::Book::from(model.clone());

        // Extract author and cover from source_data
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

    // 4. Sort Results by Relevance
    // Prioritize:
    // 1. Language matches user preference
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

    // Source boost for French users: Prioritize BNF (national library) for French content
    if user_lang == "fr" || user_lang == "fra" || user_lang == "fre" {
        if let Some(source) = &book.source {
            if source == "BNF" {
                score += 50; // National library bonus for French users
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
                // macOS: Application Support/BiblioGenius or Documents (Flutter uses getApplicationDocumentsDirectory)
                // Flutter on macOS sandboxed app uses: ~/Library/Containers/com.example.bibliogeniusApp/Data/Documents/
                // Flutter on macOS debug uses: ~/Library/Application Support/bibliogenius.db
                // For compatibility, try Documents first (where Flutter puts it)
                format!("sqlite://{}/Documents/bibliogenius.db?mode=rwc", home)
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
