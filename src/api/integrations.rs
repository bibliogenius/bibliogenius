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
}

pub async fn search_external(query: &crate::api::search::SearchQuery) -> Vec<book::Model> {
    let mut books = Vec::new();
    let client = reqwest::Client::new();

    // Build Open Library Query
    let mut q_parts = Vec::new();
    if let Some(t) = &query.title {
        q_parts.push(format!("title:{}", t));
    }
    if let Some(a) = &query.author {
        q_parts.push(format!("author:{}", a));
    }

    if q_parts.is_empty() {
        return books;
    }

    let q_str = q_parts.join(" AND ");
    let url = format!(
        "https://openlibrary.org/search.json?q={}&limit=5",
        urlencoding::encode(&q_str)
    );

    if let Ok(res) = client.get(&url).send().await {
        if let Ok(data) = res.json::<OpenLibrarySearchResponse>().await {
            for doc in data.docs {
                let isbn = doc.isbn.as_ref().and_then(|v| v.first()).cloned();

                // Map to our Book Model (store additional data in source_data)
                let source_data = serde_json::json!({
                    "authors": doc.author_name.unwrap_or_default(),
                    "cover_id": doc.cover_i,
                    "source": "openlibrary"
                });

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
                    reading_status: "to_read".to_string(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
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

pub async fn search_openlibrary(
    Query(params): Query<OpenLibraryQuery>,
) -> impl IntoResponse {
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
