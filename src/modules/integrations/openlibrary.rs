use crate::inventaire_client::AuthorMetadata;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<AuthorMetadata>,
    pub publisher: Option<String>,
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryResponse {
    #[serde(flatten)]
    books: HashMap<String, OpenLibraryBook>,
}

#[derive(Debug, Deserialize)]
struct OpenLibraryBook {
    title: String,
    authors: Option<Vec<OpenLibraryAuthor>>,
    publishers: Option<Vec<OpenLibraryPublisher>>,
    publish_date: Option<String>,
    cover: Option<OpenLibraryCover>,
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

pub async fn fetch_book_metadata(isbn: &str) -> Result<BookMetadata, String> {
    let url = format!(
        "https://openlibrary.org/api/books?bibkeys=ISBN:{}&format=json&jscmd=data",
        isbn
    );

    let client = reqwest::Client::builder()
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

    let parsed: OpenLibraryResponse =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse JSON: {}", e))?;

    let key = format!("ISBN:{}", isbn);
    if let Some(book) = parsed.books.get(&key) {
        let authors = book
            .authors
            .as_ref()
            .map(|a| {
                a.iter()
                    .filter(|auth| {
                        let n = auth.name.trim();
                        !n.eq_ignore_ascii_case("unknown author")
                            && !n.eq_ignore_ascii_case("unknown")
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

        // Fetch description from edition/work API
        let summary = fetch_description(isbn).await;

        Ok(BookMetadata {
            title: book.title.clone(),
            authors,
            publisher,
            publication_year: book.publish_date.clone(),
            cover_url,
            summary,
        })
    } else {
        Err("Book not found".to_string())
    }
}

/// Fetch description from Open Library edition and/or work API.
/// Tries edition-level description first, then follows to the parent work.
async fn fetch_description(isbn: &str) -> Option<String> {
    let client = reqwest::Client::builder()
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

    let client = reqwest::Client::new();
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

/// Fetch cover URL from OpenLibrary's Cover API (most reliable endpoint).
/// Uses `?default=false` so OpenLibrary returns 404 for missing covers
/// instead of redirecting to a 1x1 transparent placeholder.
pub async fn fetch_cover_url(isbn: &str) -> Option<String> {
    let cover_url = format!("https://covers.openlibrary.org/b/isbn/{}-L.jpg", isbn);
    let check_url = format!("{}?default=false", &cover_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    match client.head(&check_url).send().await {
        Ok(resp) if resp.status().is_success() => Some(cover_url),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
