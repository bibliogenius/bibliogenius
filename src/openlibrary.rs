use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct BookMetadata {
    pub title: String,
    pub authors: Vec<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
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

    let client = reqwest::Client::new();
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
            .map(|a| a.iter().map(|auth| auth.name.clone()).collect())
            .unwrap_or_default();

        let publisher = book
            .publishers
            .as_ref()
            .and_then(|p| p.first().map(|publ| publ.name.clone()));

        let cover_url = book
            .cover
            .as_ref()
            .and_then(|c| c.large.clone().or(c.medium.clone()));

        Ok(BookMetadata {
            title: book.title.clone(),
            authors,
            publisher,
            publication_year: book.publish_date.clone(),
            cover_url,
        })
    } else {
        Err("Book not found".to_string())
    }
}
