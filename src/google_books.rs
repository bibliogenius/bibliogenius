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
