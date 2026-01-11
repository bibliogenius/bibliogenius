use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct InventaireMetadata {
    pub title: String,
    pub authors: Vec<AuthorMetadata>,
    pub publisher: Option<String>,
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
    pub inventaire_uri: String,
    pub summary: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuthorMetadata {
    pub name: String,
    pub birth_year: Option<String>,
    pub death_year: Option<String>,
    pub image_url: Option<String>,
    pub bio: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InventaireResponse {
    pub entities: HashMap<String, InventaireEntity>,
}

#[derive(Debug, Deserialize)]
pub struct InventaireEntity {
    pub claims: Claims,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub descriptions: HashMap<String, String>,
    pub uri: String, // Was _uri, but API returns "uri"
}

#[derive(Debug, Deserialize)]
pub struct Claims {
    #[serde(rename = "wdt:P1476")] // Title
    pub title: Option<Vec<String>>,
    #[serde(rename = "wdt:P50")] // Author
    pub authors: Option<Vec<String>>, // URIs like "wd:Q123"
    #[serde(rename = "wdt:P123")] // Publisher
    pub publisher: Option<Vec<String>>,
    #[serde(rename = "wdt:P577")] // Publication Date
    pub publication_date: Option<Vec<String>>,
    #[serde(rename = "wdt:P18", alias = "invp:P2")] // Image (wdt:P18 or invp:P2)
    pub image: Option<Vec<String>>,
    #[serde(rename = "wdt:P629")] // Work (Parent)
    pub work: Option<Vec<String>>,
    #[serde(rename = "wdt:P569")] // Birth Date
    pub birth_date: Option<Vec<String>>,
    #[serde(rename = "wdt:P570")] // Death Date
    pub death_date: Option<Vec<String>>,
    #[serde(rename = "wdt:P212")] // ISBN-13
    pub isbn_13: Option<Vec<String>>,
    #[serde(rename = "wdt:P957")] // ISBN-10
    pub isbn_10: Option<Vec<String>>,
}

const USER_AGENT: &str = "BiblioGenius/1.0 (federico@bibliogenius.org)";

pub async fn fetch_inventaire_metadata(isbn: &str) -> Result<InventaireMetadata, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    // 1. Fetch Edition (ISBN)
    let edition_uri = format!("isbn:{}", isbn);
    let edition_entity = fetch_entity(&client, &edition_uri).await?;

    let title = edition_entity
        .claims
        .title
        .as_ref()
        .and_then(|v| v.first().cloned())
        .unwrap_or_else(|| "Unknown Title".to_string());

    let publication_year = edition_entity
        .claims
        .publication_date
        .and_then(|v| v.first().cloned())
        .map(|d| d.chars().take(4).collect());

    let cover_url = edition_entity
        .claims
        .image
        .as_ref()
        .and_then(|v| v.first().cloned()) // "hash"
        .map(|hash| format!("https://inventaire.io/img/entities/{}", hash));

    // 2. Get Work URI to find Author
    let work_uri = edition_entity
        .claims
        .work
        .as_ref()
        .and_then(|v| v.first().cloned());

    let mut authors = Vec::new();
    let mut work_entity_opt = None;

    if let Some(uri) = work_uri {
        // 3. Fetch Work
        if let Ok(work_entity) = fetch_entity(&client, &uri).await {
            // 4. Get Author URIs
            if let Some(author_uris) = work_entity.claims.authors.as_ref() {
                for author_uri in author_uris {
                    // 5. Fetch Author
                    if let Ok(author_entity) = fetch_entity(&client, author_uri).await {
                        // Extract Name
                        let name = author_entity
                            .labels
                            .get("fr")
                            .or_else(|| author_entity.labels.get("en"))
                            .or_else(|| author_entity.labels.values().next())
                            .cloned()
                            .unwrap_or_else(|| "Unknown Author".to_string());

                        // Extract Dates
                        let birth_year = author_entity
                            .claims
                            .birth_date
                            .as_ref()
                            .and_then(|v| v.first().cloned())
                            .map(|d| d.chars().take(4).collect());

                        let death_year = author_entity
                            .claims
                            .death_date
                            .as_ref()
                            .and_then(|v| v.first().cloned())
                            .map(|d| d.chars().take(4).collect());

                        // Extract Image
                        let image_url = author_entity
                            .claims
                            .image
                            .as_ref()
                            .and_then(|v| v.first().cloned())
                            .map(|hash| format!("https://inventaire.io/img/entities/{}", hash));

                        // Extract Bio (Description)
                        let bio = author_entity
                            .descriptions
                            .get("fr")
                            .or_else(|| author_entity.descriptions.get("en"))
                            .or_else(|| author_entity.descriptions.values().next())
                            .cloned();

                        authors.push(AuthorMetadata {
                            name,
                            birth_year,
                            death_year,
                            image_url,
                            bio,
                        });
                    }
                }
            }
            work_entity_opt = Some(work_entity);
        }
    }

    Ok(InventaireMetadata {
        title,
        authors,
        publisher: edition_entity
            .claims
            .publisher
            .as_ref()
            .and_then(|v| v.first().cloned()),
        publication_year,
        cover_url,
        summary: get_summary(
            &edition_entity.labels,
            &edition_entity.descriptions,
            work_entity_opt.as_ref(),
        ),
        inventaire_uri: format!("https://inventaire.io/entity/{}", edition_uri),
    })
}

fn get_summary(
    _edition_labels: &HashMap<String, String>,
    edition_descriptions: &HashMap<String, String>,
    work: Option<&InventaireEntity>,
) -> Option<String> {
    // Helper to pick best language
    let pick_lang = |map: &HashMap<String, String>| -> Option<String> {
        map.get("fr")
            .or_else(|| map.get("en"))
            .or_else(|| map.values().next())
            .cloned()
    };

    // Try Work first (usually has the abstract/description), then Edition
    work.and_then(|e| pick_lang(&e.descriptions))
        .or_else(|| pick_lang(edition_descriptions))
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
                assert!(metadata.authors.iter().any(|a| a.name == "Jack London"));
                assert!(metadata.authors.iter().any(|a| a.name == "Jack London"));
                assert_eq!(metadata.publication_year, Some("1999".to_string()));
                assert!(
                    metadata.summary.is_some(),
                    "Summary should not be empty for Martin Eden"
                );
                println!("Summary: {:?}", metadata.summary);
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

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Request failed for {}: {}", uri, e))?;

    if !resp.status().is_success() {
        return Err(format!("API error {}: {}", uri, resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Read body failed: {}", e))?;

    let parsed: InventaireResponse =
        serde_json::from_str(&body).map_err(|e| format!("Parse error for {}: {}", uri, e))?;

    parsed
        .entities
        .into_iter()
        .next()
        .map(|(_, entity)| entity)
        .ok_or_else(|| format!("Entity not found: {}", uri))
}

#[derive(Debug, Deserialize)]
struct InventaireSearchResponse {
    results: Vec<InventaireSearchResult>,
}

#[derive(Debug, Serialize, Deserialize)] // Added Serialize for re-use in API response if needed
pub struct InventaireSearchResult {
    pub uri: String,
    pub label: String,
    pub description: Option<String>,
    pub image: Option<String>,
    pub authors: Option<Vec<String>>, // Added authors field
    pub isbn: Option<String>,         // ISBN from first edition
}

pub async fn search_inventaire(query: &str) -> Result<Vec<InventaireSearchResult>, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    let url = format!(
        "https://inventaire.io/api/search?types=works&search={}",
        urlencoding::encode(query)
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("API error: {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("Read body failed: {}", e))?;

    let parsed: InventaireSearchResponse =
        serde_json::from_str(&body).map_err(|e| format!("Parse error: {}", e))?;

    // Fix image URLs to be absolute and initialize isbn to None
    let results = parsed
        .results
        .into_iter()
        .map(|mut item| {
            if let Some(img) = item.image {
                if !img.starts_with("http") {
                    item.image = Some(format!("https://inventaire.io{}", img));
                } else {
                    item.image = Some(img);
                }
            }
            // ISBN will be populated by enrich_search_results
            if item.isbn.is_none() {
                item.isbn = None;
            }
            item
        })
        .collect();

    Ok(results)
}

pub async fn enrich_search_results(
    results: Vec<InventaireSearchResult>,
) -> Result<Vec<InventaireSearchResult>, String> {
    if results.is_empty() {
        return Ok(results);
    }

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    // 1. Collect Work URIs
    let work_uris: Vec<String> = results.iter().map(|r| r.uri.clone()).collect();
    // Dedup not strictly necessary as search results are distinct works usually, but good practice
    // but here we just pass the vector.

    // 2. Fetch Works
    let works_map = fetch_entities_batch(&client, &work_uris).await?;

    // 3. Collect Author URIs from Works
    let mut author_uris: Vec<String> = works_map
        .values()
        .filter_map(|w| w.claims.authors.as_ref())
        .flatten()
        .cloned()
        .collect();
    author_uris.sort();
    author_uris.dedup();

    // 4. Fetch Authors
    let authors_map = if !author_uris.is_empty() {
        fetch_entities_batch(&client, &author_uris).await?
    } else {
        HashMap::new()
    };

    // 5. Enrich Results (Construct new list with Author info in label or description)
    // Note: InventaireSearchResult doesn't have an 'author' field yet,
    // so we will append it to the label or description for now,
    // OR we can update the struct.
    // The previous task `Generic Search` didn't update `InventaireSearchResult` to have specific fields
    // but the `search_unified` converts it to `Book` DTO.
    // So better to return a struct that gives us the author name so `search_unified` can use it.
    // For now, let's just append "[Author Name]" to the description if missing, or we can use a wrapper.
    // Actually, `InventaireSearchResult` is defined right above. Let's add an optional `authors` field to it
    // so we can pass structured data back to `search_unified`.

    // Wait, I can't easily change the struct definition in this replacement block without changing the struct definition lines.
    // The struct definition is at line 267.
    // Let's modify the struct definition first or simpler:
    // We can modify the `description` field to include the author name if we don't want to break things
    // or change the struct in a separate step.
    // BUT the best way is to update the struct.
    // However, since I am in `replace_file_content` for the end of the file, I might miss the struct def.
    // Let's start by adding the helper functions, and I will do a separate edit to update the struct if needed.
    // Actually, I can just return a slightly different generic or reuse the same one and put author in `description`.
    // NO, `search_unified` parses `InventaireSearchResult`.
    // Let's look at `search_unified` in `api/integrations.rs`:
    // `let book = book::Book { ... summary: item.description ... }`
    // If I put the author in the description, it will show up in summary.
    // Ideally I want `author` field.
    // Let's stick to the plan: "Returns enriched list with author names populated."
    // I will assume I can update the struct in a prior or subsequent step.
    // actually, let's use `multi_replace` to update the struct and add the functions in one go.
    // But here I'm using `replace_file_content`.
    // I will return the results as is, but I will modify the logic to separate author extraction in `search_unified`.
    // better: modify `InventaireSearchResult` to have `authors: Option<Vec<String>>`?
    // Let's check line 267.

    // For now, let's just implement `fetch_entities_batch` and `enrich_search_results` that returns `Vec<InventaireSearchResult>`
    // expecting `InventaireSearchResult` to have `author_names`?
    // Let's stick to using `description` for now as a fallback or return a new internal struct?
    // Actually, `search_unified` maps `item.label` to `title`.
    // If I change `InventaireSearchResult`, I need to update `search_unified` anyway.

    // Let's just create the `fetch_entities_batch` first.

    let mut enriched_results = Vec::new();

    for mut result in results {
        if let Some(work_entity) = works_map.get(&result.uri) {
            if let Some(uris) = &work_entity.claims.authors {
                let mut names = Vec::new();
                for uri in uris {
                    if let Some(author_entity) = authors_map.get(uri) {
                        let name = author_entity
                            .labels
                            .get("fr")
                            .or_else(|| author_entity.labels.get("en"))
                            .or_else(|| author_entity.labels.values().next())
                            .cloned()
                            .unwrap_or_else(|| "Unknown".to_string());
                        names.push(name);
                    }
                }
                if !names.is_empty() {
                    result.authors = Some(names);
                }
            }
        }
        enriched_results.push(result);
    }

    // 6. Fetch editions to get ISBNs for each work
    // Use reverse-claims to find editions that have this work as parent
    for result in &mut enriched_results {
        if result.isbn.is_some() {
            continue; // Already has ISBN
        }

        // Fetch editions for this work using reverse-claims API
        let editions_url = format!(
            "https://inventaire.io/api/entities?action=reverse-claims&property=wdt:P629&value={}",
            urlencoding::encode(&result.uri)
        );

        if let Ok(resp) = client.get(&editions_url).send().await {
            if resp.status().is_success() {
                if let Ok(body) = resp.text().await {
                    // Response is { "uris": ["inv:xxx", "inv:yyy"] }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        if let Some(uris) = json.get("uris").and_then(|u| u.as_array()) {
                            // Take first few edition URIs to fetch
                            let edition_uris: Vec<String> = uris
                                .iter()
                                .take(3) // Limit to first 3 editions
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect();

                            if !edition_uris.is_empty() {
                                // Fetch edition entities to get ISBN
                                if let Ok(edition_entities) =
                                    fetch_entities_batch(&client, &edition_uris).await
                                {
                                    for (_, entity) in edition_entities {
                                        // Try ISBN-13 first, then ISBN-10
                                        if let Some(isbn) = entity
                                            .claims
                                            .isbn_13
                                            .as_ref()
                                            .and_then(|v| v.first().cloned())
                                            .or_else(|| {
                                                entity
                                                    .claims
                                                    .isbn_10
                                                    .as_ref()
                                                    .and_then(|v| v.first().cloned())
                                            })
                                        {
                                            result.isbn = Some(isbn);
                                            break; // Found an ISBN, stop
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(enriched_results)
}

pub async fn fetch_entities_batch(
    client: &reqwest::Client,
    uris: &[String],
) -> Result<HashMap<String, InventaireEntity>, String> {
    if uris.is_empty() {
        return Ok(HashMap::new());
    }

    // Chunking to avoid URL too long (50 max usually safe)
    let chunks = uris.chunks(50);
    let mut all_entities = HashMap::new();

    for chunk in chunks {
        let joined = chunk.join("|");
        let url = format!(
            "https://inventaire.io/api/entities?action=by-uris&uris={}",
            joined
        );

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Batch request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("Batch API error: {}", resp.status()));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| format!("Read batch body failed: {}", e))?;

        let parsed: InventaireResponse =
            serde_json::from_str(&body).map_err(|e| format!("Batch parse error: {}", e))?;

        all_entities.extend(parsed.entities);
    }

    Ok(all_entities)
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[tokio::test]
    async fn test_search_inventaire() {
        let query = "Harry Potter";
        let result = search_inventaire(query).await;

        match result {
            Ok(results) => {
                println!("Found {} results", results.len());
                assert!(!results.is_empty());
                let first = &results[0];
                println!("First result: {:?}", first);
                assert!(first.label.contains("Harry Potter"));
            }
            Err(e) => {
                // It's possible the API fails in CI/offline, but for manual verification it should pass.
                panic!("Search failed: {}", e);
            }
        }
    }
}
