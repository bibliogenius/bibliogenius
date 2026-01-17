use futures::stream::{self, StreamExt};
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
    /// Image object returned directly on entity (not in claims)
    /// Format: { "url": "/img/entities/HASH" }
    #[serde(default)]
    pub image: Option<InventaireImage>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct InventaireImage {
    /// URL may be absent if the image object is empty `{}`
    pub url: Option<String>,
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
    #[serde(rename = "wdt:P407")] // Language of work (Wikidata URI like "wd:Q150" for French)
    pub language: Option<Vec<String>>,
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
        .as_ref()
        .and_then(|v| v.first().cloned())
        .map(|d| d.chars().take(4).collect());

    let cover_url = get_entity_image_url(&edition_entity);

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
                        let image_url = get_entity_image_url(&author_entity);

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

    // Resolve publisher from Wikidata URI to human-readable name
    let publisher = if let Some(publisher_uri) = edition_entity
        .claims
        .publisher
        .as_ref()
        .and_then(|v| v.first().cloned())
    {
        // If it's a Wikidata URI (wd:Qxxx), fetch the entity to get the name
        if publisher_uri.starts_with("wd:")
            && let Ok(publisher_entity) = fetch_entity(&client, &publisher_uri).await
        {
            publisher_entity
                .labels
                .get("fr")
                .or_else(|| publisher_entity.labels.get("en"))
                .or_else(|| publisher_entity.labels.values().next())
                .cloned()
        } else {
            Some(publisher_uri)
        }
    } else {
        None
    };

    Ok(InventaireMetadata {
        title,
        authors,
        publisher,
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
    pub publisher: Option<String>,    // Publisher name (resolved from Wikidata URI)
    pub language: Option<String>,     // Language code (e.g., "fr", "en")
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
        if let Some(work_entity) = works_map.get(&result.uri)
            && let Some(uris) = &work_entity.claims.authors
        {
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
        enriched_results.push(result);
    }

    // 6. Fetch editions for each work and create separate results per edition
    // Use parallel stream processing to fetch editions (limit concurrency to 5)
    let final_results: Vec<InventaireSearchResult> = stream::iter(enriched_results)
        .map(|result| {
            let client = client.clone();
            async move {
                // Fetch editions for this work using reverse-claims API
                let editions_url = format!(
                    "https://inventaire.io/api/entities?action=reverse-claims&property=wdt:P629&value={}",
                    urlencoding::encode(&result.uri)
                );

                let mut work_results = Vec::new(); // Results for this specific work item
                let mut found_editions = false;

                if let Ok(resp) = client.get(&editions_url).send().await
                    && resp.status().is_success()
                    && let Ok(body) = resp.text().await
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
                    && let Some(uris) = json.get("uris").and_then(|u| u.as_array())
                {
                                    // Fetch up to 40 editions per work
                                    let edition_uris: Vec<String> = uris
                                        .iter()
                                        .take(40)
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect();

                                    if !edition_uris.is_empty() {
                                        // Fetch edition entities
                                        if let Ok(edition_entities) =
                                            fetch_entities_batch(&client, &edition_uris).await
                                        {
                                            // Collect all publisher URIs to batch fetch
                                            let publisher_uris: Vec<String> = edition_entities
                                                .values()
                                                .filter_map(|e| {
                                                    e.claims
                                                        .publisher
                                                        .as_ref()
                                                        .and_then(|v| v.first().cloned())
                                                })
                                                .filter(|uri| uri.starts_with("wd:"))
                                                .collect();

                                            // Batch fetch publishers
                                            let publishers_map = if !publisher_uris.is_empty() {
                                                fetch_entities_batch(&client, &publisher_uris)
                                                    .await
                                                    .unwrap_or_default()
                                            } else {
                                                HashMap::new()
                                            };

                                            // Create one result per edition
                                            for (edition_uri, entity) in &edition_entities {
                                                // Get ISBN (prefer ISBN-13)
                                                let isbn = entity
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
                                                    });

                                                // Get publisher name
                                                let publisher = entity
                                                    .claims
                                                    .publisher
                                                    .as_ref()
                                                    .and_then(|v| v.first())
                                                    .and_then(|pub_uri| {
                                                        if pub_uri.starts_with("wd:") {
                                                            publishers_map.get(pub_uri).and_then(|pe| {
                                                                pe.labels
                                                                    .get("fr")
                                                                    .or_else(|| pe.labels.get("en"))
                                                                    .or_else(|| pe.labels.values().next())
                                                                    .cloned()
                                                            })
                                                        } else {
                                                            Some(pub_uri.clone())
                                                        }
                                                    });

                                                // Get language
                                                let language = entity
                                                    .claims
                                                    .language
                                                    .as_ref()
                                                    .and_then(|v| v.first())
                                                    .map(|uri| wikidata_language_to_code(uri));

                                                // Get edition title (might differ from work title)
                                                let edition_title = entity
                                                    .labels
                                                    .get("fr")
                                                    .or_else(|| entity.labels.get("en"))
                                                    .or_else(|| entity.labels.values().next())
                                                    .cloned();

                                                // Get edition cover image
                                                let edition_image = get_entity_image_url(entity);

                                                let edition_result = InventaireSearchResult {
                                                    uri: edition_uri.clone(),
                                                    label: edition_title.unwrap_or_else(|| result.label.clone()),
                                                    description: result.description.clone(),
                                                    image: edition_image.or_else(|| result.image.clone()),
                                                    authors: result.authors.clone(),
                                                    isbn: isbn.clone(),
                                                    publisher: publisher.clone(),
                                                    language,
                                                };

                                                // Quality Filter: Only include if it has at least one of (ISBN, Cover, Publisher)
                                                if edition_result.isbn.is_some() ||
                                                   edition_result.image.is_some() ||
                                                   edition_result.publisher.is_some() {
                                                    work_results.push(edition_result);
                                                    found_editions = true;
                                                }
                                            }
                                        }
                                    }
                }

                if !found_editions {
                    // Start Filter: Discard poor quality results (no cover AND no ISBN AND no publisher)
                    let has_cover = result.image.is_some();
                    let has_isbn = result.isbn.is_some();
                    let has_publisher = result.publisher.is_some();

                    if has_cover || has_isbn || has_publisher {
                        work_results.push(result);
                    }
                    // End Filter
                }
                work_results
            }
        })
        .buffer_unordered(5) // Run up to 5 unrelated work edition fetches in parallel
        .collect::<Vec<Vec<InventaireSearchResult>>>()
        .await
        .into_iter()
        .flatten()
        .collect();

    Ok(final_results)
}

/// Convert Wikidata language URI to ISO 639-1 language code
fn wikidata_language_to_code(uri: &str) -> String {
    // Common Wikidata language QIDs
    match uri {
        "wd:Q150" => "fr".to_string(),    // French
        "wd:Q1860" => "en".to_string(),   // English
        "wd:Q1321" => "es".to_string(),   // Spanish
        "wd:Q188" => "de".to_string(),    // German
        "wd:Q652" => "it".to_string(),    // Italian
        "wd:Q5146" => "pt".to_string(),   // Portuguese
        "wd:Q7411" => "nl".to_string(),   // Dutch
        "wd:Q7737" => "ru".to_string(),   // Russian
        "wd:Q5287" => "ja".to_string(),   // Japanese
        "wd:Q7850" => "zh".to_string(),   // Chinese
        "wd:Q9288" => "he".to_string(),   // Hebrew
        "wd:Q9299" => "sr".to_string(),   // Serbian
        "wd:Q9072" => "eu".to_string(),   // Basque
        "wd:Q7026" => "ca".to_string(),   // Catalan
        "wd:Q9027" => "sv".to_string(),   // Swedish
        "wd:Q9035" => "da".to_string(),   // Danish
        "wd:Q9043" => "no".to_string(),   // Norwegian
        "wd:Q9056" => "cs".to_string(),   // Czech
        "wd:Q9067" => "hu".to_string(),   // Hungarian
        "wd:Q9058" => "pl".to_string(),   // Polish
        "wd:Q9083" => "el".to_string(),   // Greek
        "wd:Q9168" => "fa".to_string(),   // Persian
        "wd:Q9217" => "ko".to_string(),   // Korean
        "wd:Q256" => "tr".to_string(),    // Turkish
        "wd:Q8798" => "uk".to_string(),   // Ukrainian
        "wd:Q9078" => "bg".to_string(),   // Bulgarian
        "wd:Q13955" => "ar".to_string(),  // Arabic
        "wd:Q9240" => "id".to_string(),   // Indonesian
        "wd:Q9252" => "vi".to_string(),   // Vietnamese
        "wd:Q9176" => "th".to_string(),   // Thai
        "wd:Q1571" => "la".to_string(),   // Latin
        "wd:Q35497" => "grc".to_string(), // Ancient Greek
        _ => {
            // If not in our map, extract the Q number as fallback
            uri.trim_start_matches("wd:").to_string()
        }
    }
}

/// Extract image URL from an Inventaire entity.
/// Priority: entity.image.url (always present when image exists) > claims.invp:P2/wdt:P18
fn get_entity_image_url(entity: &InventaireEntity) -> Option<String> {
    // First try the image object on the entity (most reliable)
    // Note: image can be an empty object `{}` with url: None
    if let Some(img) = &entity.image
        && let Some(url) = &img.url
    {
        if url.starts_with("http") {
            return Some(url.clone());
        } else if url.starts_with("/") {
            return Some(format!("https://inventaire.io{}", url));
        } else {
            return Some(format!("https://inventaire.io/img/entities/{}", url));
        }
    }

    // Fallback to claims (for older API responses or edge cases)
    entity
        .claims
        .image
        .as_ref()
        .and_then(|v| v.first().cloned())
        .map(|hash| {
            if hash.starts_with("http") {
                hash
            } else if hash.starts_with("/") {
                format!("https://inventaire.io{}", hash)
            } else {
                format!("https://inventaire.io/img/entities/{}", hash)
            }
        })
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
    #[ignore] // Flaky in CI due to external network request
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

    #[tokio::test]
    #[ignore] // Flaky in CI due to external network request
    async fn test_search_with_enrichment() {
        let query = "Martin Eden";
        let search_result = search_inventaire(query).await;

        match search_result {
            Ok(results) => {
                println!("Search returned {} results", results.len());
                assert!(!results.is_empty());

                // Now enrich the results
                match enrich_search_results(results).await {
                    Ok(enriched) => {
                        println!("Enrichment returned {} results", enriched.len());
                        assert!(
                            !enriched.is_empty(),
                            "Enrichment should return at least one result"
                        );

                        // Check that at least some results have ISBNs
                        let with_isbn = enriched.iter().filter(|r| r.isbn.is_some()).count();
                        println!("Results with ISBN: {}", with_isbn);

                        // Check that at least some results have authors
                        let with_authors = enriched.iter().filter(|r| r.authors.is_some()).count();
                        println!("Results with authors: {}", with_authors);

                        // Print first few results for debugging
                        for (i, result) in enriched.iter().take(5).enumerate() {
                            println!(
                                "Result {}: {} | ISBN: {:?} | Authors: {:?} | Publisher: {:?}",
                                i + 1,
                                result.label,
                                result.isbn,
                                result.authors,
                                result.publisher
                            );
                        }

                        assert!(with_isbn > 0, "At least one result should have an ISBN");
                    }
                    Err(e) => {
                        panic!("Enrichment failed: {}", e);
                    }
                }
            }
            Err(e) => {
                panic!("Search failed: {}", e);
            }
        }
    }
}
