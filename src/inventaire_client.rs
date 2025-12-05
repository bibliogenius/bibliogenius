use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct InventaireMetadata {
    pub title: String,
    pub authors: Vec<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
    pub inventaire_uri: String,
}

#[derive(Debug, Deserialize)]
struct InventaireResponse {
    entities: HashMap<String, InventaireEntity>,
}

#[derive(Debug, Deserialize)]
struct InventaireEntity {
    claims: Claims,
    #[serde(default)]
    labels: HashMap<String, String>,
    uri: String,
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(rename = "wdt:P1476")] // Title
    title: Option<Vec<String>>,
    #[serde(rename = "wdt:P50")] // Author
    authors: Option<Vec<String>>, // URIs like "wd:Q123"
    #[serde(rename = "wdt:P123")] // Publisher
    publisher: Option<Vec<String>>,
    #[serde(rename = "wdt:P577")] // Publication Date
    publication_date: Option<Vec<String>>,
    #[serde(rename = "wdt:P18", alias = "invp:P2")] // Image (wdt:P18 or invp:P2)
    image: Option<Vec<String>>,
    #[serde(rename = "wdt:P629")] // Work (Parent)
    work: Option<Vec<String>>,
}

// Note: Inventaire API returns entities with claims.
// The structure is complex because it's RDF-like (Wikidata style).
// For a simple MVP, we might need to fetch the "hydrated" version or handle the URIs.
// Inventaire has an endpoint `https://api.inventaire.io/entities?action=by-uris&uris=isbn:978...`
// But the response gives us URIs for authors, not names directly unless we ask for them.
// A better endpoint might be `https://inventaire.io/api/entities?action=by-uris&uris=isbn:978...&compact=true` or similar?
// Let's stick to the raw one and see what we get, or use their "search" endpoint which might be friendlier.

// Actually, looking at the conference transcript, they use Wikidata IDs.
// Let's try to implement a basic fetcher that gets the raw data first.


const USER_AGENT: &str = "BiblioGenius/1.0 (federico@bibliogenius.org)";

pub async fn fetch_inventaire_metadata(isbn: &str) -> Result<InventaireMetadata, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    // 1. Fetch Edition (ISBN)
    let edition_uri = format!("isbn:{}", isbn);
    let edition_entity = fetch_entity(&client, &edition_uri).await?;

    let title = edition_entity.claims.title
        .and_then(|v| v.first().cloned())
        .unwrap_or_else(|| "Unknown Title".to_string());

    let publication_year = edition_entity.claims.publication_date
        .and_then(|v| v.first().cloned())
        .map(|d| d.chars().take(4).collect());

    let cover_url = edition_entity.claims.image
        .and_then(|v| v.first().cloned()) // "hash"
        .map(|hash| format!("https://inventaire.io/img/entities/{}", hash));
    
    // 2. Get Work URI to find Author
    let work_uri = edition_entity.claims.work
        .and_then(|v| v.first().cloned());

    let mut authors = Vec::new();

    if let Some(uri) = work_uri {
        // 3. Fetch Work
        if let Ok(work_entity) = fetch_entity(&client, &uri).await {
            // 4. Get Author URIs
            if let Some(author_uris) = work_entity.claims.authors {
                for author_uri in author_uris {
                    // 5. Fetch Author
                    if let Ok(author_entity) = fetch_entity(&client, &author_uri).await {
                        // Try to get label in English or French, fallback to any
                        let name = author_entity.labels.get("fr")
                            .or_else(|| author_entity.labels.get("en"))
                            .or_else(|| author_entity.labels.values().next())
                            .cloned()
                            .unwrap_or_else(|| "Unknown Author".to_string());
                        authors.push(name);
                    }
                }
            }
        }
    }

    Ok(InventaireMetadata {
        title,
        authors,
        publisher: edition_entity.claims.publisher.and_then(|v| v.first().cloned()),
        publication_year,
        cover_url,
        inventaire_uri: format!("https://inventaire.io/entity/{}", edition_uri),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fetch_inventaire_metadata() {
        // Martin Eden (ISBN from the CSV export)
        let isbn = "9782264024848";
        let result = fetch_inventaire_metadata(isbn).await;

        match result {
            Ok(metadata) => {
                println!("Fetched metadata: {:?}", metadata);
                assert_eq!(metadata.title, "Martin Eden");
                assert!(metadata.authors.contains(&"Jack London".to_string()));
                assert_eq!(metadata.publication_year, Some("1999".to_string()));
            }
            Err(e) => {
                panic!("Failed to fetch metadata: {}", e);
            }
        }
    }
}

async fn fetch_entity(client: &reqwest::Client, uri: &str) -> Result<InventaireEntity, String> {
    let url = format!(
        "https://inventaire.io/api/entities?action=by-uris&uris={}",
        uri
    );

    let resp = client.get(&url).send().await
        .map_err(|e| format!("Request failed for {}: {}", uri, e))?;

    if !resp.status().is_success() {
        return Err(format!("API error {}: {}", uri, resp.status()));
    }

    let body = resp.text().await
        .map_err(|e| format!("Read body failed: {}", e))?;

    let parsed: InventaireResponse = serde_json::from_str(&body)
        .map_err(|e| format!("Parse error for {}: {}", uri, e))?;

    parsed.entities.into_iter().next()
        .map(|(_, entity)| entity)
        .ok_or_else(|| format!("Entity not found: {}", uri))
}

