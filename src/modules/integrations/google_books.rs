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
        let info = &first_item.volume_info;

        let authors = info
            .authors
            .as_ref()
            .map(|list| {
                list.iter()
                    .filter(|name| {
                        let n = name.trim();
                        !n.eq_ignore_ascii_case("unknown author")
                            && !n.eq_ignore_ascii_case("unknown")
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

        return Ok(BookMetadata {
            title: info.title.clone(),
            authors,
            publisher: info.publisher.clone(),
            publication_year: info.published_date.clone(),
            cover_url,
            summary: info.description.clone(),
        });
    }

    Err("Book not found in Google Books".to_string())
}

pub async fn fetch_cover_url(isbn: &str, api_key: Option<&str>) -> Option<String> {
    let base_url = format!(
        "https://www.googleapis.com/books/v1/volumes?q=isbn:{}",
        isbn
    );
    let url = append_api_key(&base_url, api_key);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;

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
        return None;
    }
    if !status.is_success() {
        tracing::warn!(
            "Google Books cover fetch error for ISBN {}: HTTP {}",
            isbn,
            status
        );
        return None;
    }

    let body = resp.text().await.ok()?;
    let parsed: GoogleBooksResponse = serde_json::from_str(&body).ok()?;

    if let Some(items) = parsed.items
        && let Some(first_item) = items.first()
        && let Some(links) = &first_item.volume_info.image_links
        && let Some(thumb) = &links.thumbnail
    {
        // Google Books returns http links often, upgrade to https
        let secure_url = thumb.replace("http://", "https://");
        return Some(secure_url);
    }

    None
}

pub async fn search_books(
    query: &crate::api::search::SearchQuery,
    api_key: Option<&str>,
) -> Vec<crate::models::book::Model> {
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
        return Vec::new();
    }

    let q_str = q_parts.join("+"); // Google Books uses + or space
    let max_results = if query.autocomplete.unwrap_or(false) {
        10 // More results for autocomplete to allow quality filtering
    } else {
        15
    };
    let base_url = format!(
        "https://www.googleapis.com/books/v1/volumes?q={}&maxResults={}",
        q_str, max_results
    );
    let url = append_api_key(&base_url, api_key);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut books = Vec::new();

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Google Books search request failed: {}", e);
            return books;
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
        return books;
    }
    if !status.is_success() {
        tracing::warn!("Google Books search error: HTTP {}", status);
        return books;
    }

    let parsed = match resp.json::<GoogleBooksResponse>().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Google Books response parse error: {}", e);
            return books;
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
                id: 0,
                title: info.title,
                isbn,
                publisher: info.publisher,
                publication_year: info
                    .published_date
                    .and_then(|d| d.chars().take(4).collect::<String>().parse().ok()),
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
            };
            books.push(book);
        }
    }

    // Deduplicate results - Google Books often returns the same book multiple times
    // (different formats like hardcover/paperback/ebook with identical data)
    let mut seen = std::collections::HashSet::new();
    books.retain(|book| {
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

    books
}
