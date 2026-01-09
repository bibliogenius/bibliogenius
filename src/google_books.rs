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
    #[serde(rename = "imageLinks")]
    image_links: Option<GoogleImageLinks>,
}

#[derive(Debug, Deserialize)]
struct GoogleImageLinks {
    thumbnail: Option<String>,
    // smallThumbnail is also available but often too small
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
