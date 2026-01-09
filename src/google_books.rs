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
    #[serde(rename = "imageLinks")]
    image_links: Option<GoogleImageLinks>,
}

#[derive(Debug, Deserialize)]
struct GoogleImageLinks {
    thumbnail: Option<String>,
    // smallThumbnail is also available but often too small
}

// Reuse definitions from openlibrary or define local mapping
use crate::inventaire_client::AuthorMetadata;
use crate::openlibrary::BookMetadata;

pub async fn fetch_book_metadata(isbn: &str) -> Result<BookMetadata, String> {
    let url = format!(
        "https://www.googleapis.com/books/v1/volumes?q=isbn:{}",
        isbn
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;

    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("Google Books API Error: {}", resp.status()));
    }

    let body = resp.text().await.map_err(|e| e.to_string())?;
    let parsed: GoogleBooksResponse = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    if let Some(items) = parsed.items {
        if let Some(first_item) = items.first() {
            let info = &first_item.volume_info;

            let authors = info
                .authors
                .as_ref()
                .map(|list| {
                    list.iter()
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
    }

    Err("Book not found in Google Books".to_string())
}

pub async fn fetch_cover_url(isbn: &str) -> Option<String> {
    let url = format!(
        "https://www.googleapis.com/books/v1/volumes?q=isbn:{}",
        isbn
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body = resp.text().await.ok()?;
    let parsed: GoogleBooksResponse = serde_json::from_str(&body).ok()?;

    if let Some(items) = parsed.items {
        if let Some(first_item) = items.first() {
            if let Some(links) = &first_item.volume_info.image_links {
                if let Some(thumb) = &links.thumbnail {
                    // Google Books returns http links often, upgrade to https
                    let secure_url = thumb.replace("http://", "https://");
                    return Some(secure_url);
                }
            }
        }
    }

    None
}

pub async fn search_books(
    query: &crate::api::search::SearchQuery,
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
    let url = format!(
        "https://www.googleapis.com/books/v1/volumes?q={}&maxResults=10",
        q_str
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut books = Vec::new();

    if let Ok(resp) = client.get(&url).send().await {
        if let Ok(parsed) = resp.json::<GoogleBooksResponse>().await {
            if let Some(items) = parsed.items {
                for item in items {
                    let info = item.volume_info;

                    // Convert to Book Model
                    let cover_url = info
                        .image_links
                        .as_ref()
                        .and_then(|l| l.thumbnail.clone())
                        .map(|url| url.replace("http://", "https://"));

                    let source_data = serde_json::json!({
                       "source": "google_books",
                       "authors": info.authors.clone().unwrap_or_default(),
                    });

                    let book = crate::models::book::Model {
                        id: 0,
                        title: info.title,
                        isbn: None, // We could try to extract ISBN from industryIdentifiers if needed
                        publisher: info.publisher,
                        publication_year: info
                            .published_date
                            .map(|d| d.chars().take(4).collect::<String>().parse().ok())
                            .flatten(),
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
                    };
                    books.push(book);
                }
            }
        }
    }
    books
}
