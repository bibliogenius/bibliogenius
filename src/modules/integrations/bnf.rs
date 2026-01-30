//! BNF (Bibliothèque nationale de France) integration via data.bnf.fr SPARQL endpoint
//!
//! This module provides search functionality for French books via the BNF's
//! Linked Open Data SPARQL endpoint.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Simple in-memory cache with TTL for BNF queries
/// Avoids repeated slow SPARQL queries for the same search terms
struct CacheEntry {
    data: Vec<BnfBook>,
    created_at: Instant,
}

static BNF_CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Cache for BNF SRU queries (catalogue.bnf.fr)
static BNF_SRU_CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour
const MAX_CACHE_ENTRIES: usize = 100; // Limit memory usage

/// A book result from BNF search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BnfBook {
    pub title: String,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub bnf_uri: String,
    pub description: Option<String>,
}

/// SPARQL response structures
#[derive(Debug, Deserialize)]
struct SparqlResponse {
    results: SparqlResults,
}

#[derive(Debug, Deserialize)]
struct SparqlResults {
    bindings: Vec<SparqlBinding>,
}

#[derive(Debug, Deserialize)]
struct SparqlBinding {
    work: Option<SparqlValue>,
    title: Option<SparqlValue>,
    #[serde(rename = "author")]
    _author: Option<SparqlValue>,
    #[serde(rename = "authorName")]
    author_name: Option<SparqlValue>,
    publisher: Option<SparqlValue>,
    date: Option<SparqlValue>,
    isbn: Option<SparqlValue>,
    description: Option<SparqlValue>,
}

#[derive(Debug, Deserialize)]
struct SparqlValue {
    value: String,
}

/// Search for books on data.bnf.fr using SPARQL
///
/// # Arguments
/// * `query` - Search query (title, author, or general search)
///
/// # Returns
/// A vector of BnfBook results
pub async fn search_bnf(query: &str) -> Result<Vec<BnfBook>, String> {
    let cache_key = query.to_lowercase().trim().to_string();

    // Check cache first (use try_lock to avoid blocking/panics)
    if let Ok(cache) = BNF_CACHE.try_lock()
        && let Some(entry) = cache.get(&cache_key)
        && entry.created_at.elapsed() < CACHE_TTL
    {
        tracing::debug!("BNF cache hit for query: {}", query);
        return Ok(entry.data.clone());
    }

    tracing::debug!("BNF cache miss for query: {}", query);
    println!("DEBUG BNF: Starting search for '{}'", query);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30)) // Increased to 30s to avoid CI timeouts
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // SPARQL query to search for books by title or author
    // Optimized with UNION to avoid slow full scans with OR filters
    let sparql_query = format!(
        r#"
PREFIX dcterms: <http://purl.org/dc/terms/>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX bnf-onto: <http://data.bnf.fr/ontology/bnf-onto/>
PREFIX rdarelationships: <http://rdvocab.info/RDARelationshipsWEMI/>

SELECT DISTINCT ?work ?title ?authorName ?publisher ?date ?isbn ?description
WHERE {{
    {{
        ?work dcterms:title ?title .
        FILTER(CONTAINS(LCASE(?title), LCASE("{search}")))
    }}
    UNION
    {{
        ?work dcterms:creator ?author .
        ?author foaf:name ?authorName .
        FILTER(CONTAINS(LCASE(?authorName), LCASE("{search}")))
        ?work dcterms:title ?title .
    }}
    
    OPTIONAL {{
        ?work dcterms:creator ?author .
        ?author foaf:name ?authorName .
    }}
    
    OPTIONAL {{
        ?work rdarelationships:expressionManifested ?manifestation .
        ?manifestation dcterms:publisher ?publisherEntity .
        ?publisherEntity foaf:name ?publisher .
    }}
    
    OPTIONAL {{
        ?work rdarelationships:expressionManifested ?manifestation .
        ?manifestation dcterms:date ?date .
    }}
    
    OPTIONAL {{
        ?work rdarelationships:expressionManifested ?manifestation .
        ?manifestation bnf-onto:isbn ?isbn .
    }}
    
    OPTIONAL {{
        ?work dcterms:description ?description .
    }}
}}
LIMIT 30
"#,
        search = query.replace('"', r#"\""#)
    );

    let response = client
        .get("https://data.bnf.fr/sparql")
        .query(&[
            ("query", sparql_query.as_str()),
            ("format", "application/sparql-results+json"),
        ])
        .send()
        .await
        .map_err(|e| format!("BNF API request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!(
            "BNF API returned error status: {}",
            response.status()
        ));
    }

    let sparql_result: SparqlResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse BNF response: {}", e))?;

    let mut books: Vec<BnfBook> = Vec::new();
    let mut seen_uris: std::collections::HashSet<String> = std::collections::HashSet::new();

    for binding in sparql_result.results.bindings {
        let uri = binding
            .work
            .as_ref()
            .map(|w| w.value.clone())
            .unwrap_or_default();

        // Deduplicate by work URI
        if seen_uris.contains(&uri) {
            continue;
        }
        seen_uris.insert(uri.clone());

        let title = binding
            .title
            .as_ref()
            .map(|t| t.value.clone())
            .unwrap_or_default();

        if title.is_empty() {
            continue;
        }

        // Parse year from date string (could be "2020", "2020-01-01", etc.)
        let year = binding.date.as_ref().and_then(|d| {
            d.value
                .split('-')
                .next()
                .and_then(|y| y.parse::<i32>().ok())
        });

        // Generate cover URL from BNF if available
        // BNF provides covers via their Gallica service for some works
        let cover_url = generate_bnf_cover_url(&uri);

        let book = BnfBook {
            title,
            author: binding.author_name.map(|a| a.value),
            publisher: binding.publisher.map(|p| p.value),
            publication_year: year,
            isbn: binding.isbn.map(|i| i.value),
            cover_url,
            bnf_uri: uri,
            description: binding.description.map(|d| d.value),
        };

        books.push(book);
    }

    // Store in cache (with LRU-style eviction if full)
    // Use try_lock to avoid blocking - cache write is optional
    if let Ok(mut cache) = BNF_CACHE.try_lock() {
        // Evict oldest entries if cache is full
        if cache.len() >= MAX_CACHE_ENTRIES {
            // Remove expired entries first
            cache.retain(|_, entry| entry.created_at.elapsed() < CACHE_TTL);

            // If still full, remove oldest entry
            if cache.len() >= MAX_CACHE_ENTRIES
                && let Some(oldest_key) = cache
                    .iter()
                    .min_by_key(|(_, e)| e.created_at)
                    .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
            }
        }

        cache.insert(
            cache_key,
            CacheEntry {
                data: books.clone(),
                created_at: Instant::now(),
            },
        );
    }

    println!("DEBUG BNF: Search completed with {} results", books.len());
    Ok(books)
}

/// Generate a potential cover URL from BNF
/// BNF doesn't always have covers, but we can try the Gallica thumbnail service
fn generate_bnf_cover_url(bnf_uri: &str) -> Option<String> {
    // Extract the ARK identifier from the URI
    // URI format: http://data.bnf.fr/ark:/12148/cb123456789
    if let Some(ark_start) = bnf_uri.find("ark:/") {
        let ark = &bnf_uri[ark_start..];
        // Only generate thumbnail URLs for digitized documents (bpt6k, btv1b)
        // Notice ARKs (cb...) don't have thumbnails on Gallica - they cause connection errors
        let ark_id = ark.trim_start_matches("ark:/12148/");
        if !ark_id.starts_with("bpt6k") && !ark_id.starts_with("btv1b") {
            return None;
        }
        // Try Gallica thumbnail service
        return Some(format!(
            "https://gallica.bnf.fr/{}/thumbnail",
            ark.replace("ark:/", "ark:")
        ));
    }
    None
}

/// Search for a book by ISBN on data.bnf.fr
pub async fn lookup_bnf_isbn(isbn: &str) -> Result<Option<BnfBook>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // SPARQL query to search for exact ISBN
    let sparql_query = format!(
        r#"
PREFIX dcterms: <http://purl.org/dc/terms/>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX bnf-onto: <http://data.bnf.fr/ontology/bnf-onto/>
PREFIX rdarelationships: <http://rdvocab.info/RDARelationshipsWEMI/>

SELECT DISTINCT ?work ?title ?authorName ?publisher ?date ?isbn ?description
WHERE {{
    ?manifestation bnf-onto:isbn "{isbn}" .
    ?work rdarelationships:expressionManifested ?manifestation ;
          dcterms:title ?title .
    
    OPTIONAL {{
        ?work dcterms:creator ?author .
        ?author foaf:name ?authorName .
    }}
    
    OPTIONAL {{
        ?manifestation dcterms:publisher ?publisherEntity .
        ?publisherEntity foaf:name ?publisher .
    }}
    
    OPTIONAL {{
        ?manifestation dcterms:date ?date .
    }}
    
    OPTIONAL {{
        ?work dcterms:description ?description .
    }}
}}
LIMIT 1
"#,
        isbn = isbn.replace('-', "")
    );

    let response = client
        .get("https://data.bnf.fr/sparql")
        .query(&[
            ("query", sparql_query.as_str()),
            ("format", "application/sparql-results+json"),
        ])
        .send()
        .await
        .map_err(|e| format!("BNF API request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!(
            "BNF API returned error status: {}",
            response.status()
        ));
    }

    let sparql_result: SparqlResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse BNF response: {}", e))?;

    if let Some(binding) = sparql_result.results.bindings.first() {
        let uri = binding
            .work
            .as_ref()
            .map(|w| w.value.clone())
            .unwrap_or_default();

        let title = binding
            .title
            .as_ref()
            .map(|t| t.value.clone())
            .unwrap_or_default();

        // Parse year from date string
        let year = binding.date.as_ref().and_then(|d| {
            d.value
                .split('-')
                .next()
                .and_then(|y| y.parse::<i32>().ok())
        });

        let cover_url = generate_bnf_cover_url(&uri);

        Ok(Some(BnfBook {
            title,
            author: binding.author_name.as_ref().map(|a| a.value.clone()),
            publisher: binding.publisher.as_ref().map(|p| p.value.clone()),
            publication_year: year,
            isbn: Some(isbn.to_string()),
            cover_url,
            bnf_uri: uri,
            description: binding.description.as_ref().map(|d| d.value.clone()),
        }))
    } else {
        Ok(None)
    }
}

/// Search for a book by ISBN on catalogue.bnf.fr (SRU API)
/// This has better coverage than the SPARQL endpoint for recent books
pub async fn lookup_bnf_sru(isbn: &str) -> Result<Option<BnfBook>, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let clean_isbn = isbn.replace('-', "");
    let cache_key = format!("isbn:{}", clean_isbn);

    // Check cache first
    if let Ok(cache) = BNF_SRU_CACHE.try_lock()
        && let Some(entry) = cache.get(&cache_key)
        && entry.created_at.elapsed() < CACHE_TTL
    {
        return Ok(entry.data.first().cloned());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let url = format!(
        "https://catalogue.bnf.fr/api/SRU?version=1.2&operation=searchRetrieve&query=bib.isbn%20adj%20%22{}%22&recordSchema=unimarcxchange",
        clean_isbn
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("BNF SRU request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("BNF SRU returned error: {}", response.status()));
    }

    let xml = response
        .text()
        .await
        .map_err(|e| format!("Failed to read BNF SRU response: {}", e))?;

    // Check if any records found
    if xml.contains("<srw:numberOfRecords>0</srw:numberOfRecords>") {
        return Ok(None);
    }

    // Parse MARC XML
    let mut reader = Reader::from_str(&xml);
    reader.trim_text(true);

    let mut title = String::new();
    let mut author: Option<String> = None;
    let mut author_surname: Option<String> = None;
    let mut author_firstname: Option<String> = None;
    let mut publisher: Option<String> = None;
    let mut year: Option<i32> = None;
    let mut description: Option<String> = None;
    let mut ark_id: Option<String> = None;

    let mut buf = Vec::new();
    let mut current_tag = String::new();
    let mut current_code = String::new();
    let mut in_datafield = false;
    let mut in_subfield = false;
    let mut in_controlfield = false;
    let mut controlfield_tag = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                if name.ends_with("datafield") {
                    in_datafield = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"tag" {
                            current_tag = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                } else if name.ends_with("subfield") && in_datafield {
                    in_subfield = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"code" {
                            current_code = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                } else if name.ends_with("controlfield") {
                    in_controlfield = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"tag" {
                            controlfield_tag = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();

                // Control fields (003 = ARK URL)
                if in_controlfield && controlfield_tag == "003" {
                    if let Some(ark_start) = text.find("ark:/12148/") {
                        ark_id = Some(text[ark_start..].to_string());
                    }
                }

                // Data fields
                if in_subfield {
                    match (current_tag.as_str(), current_code.as_str()) {
                        // Title (200 $a)
                        ("200", "a") => title = text,
                        // Author from 200 $f (simple form)
                        ("200", "f") if author.is_none() => author = Some(text),
                        // Author from 700 $a (surname) and $b (firstname)
                        ("700", "a") | ("701", "a") => author_surname = Some(text),
                        ("700", "b") | ("701", "b") => author_firstname = Some(text),
                        // Publisher (214 $c for new UNIMARC, 210 $c for old)
                        ("214", "c") | ("210", "c") => publisher = Some(text),
                        // Year (214 $d or 210 $d) - extract 4 digits
                        ("214", "d") | ("210", "d") => {
                            year = text
                                .chars()
                                .filter(|c| c.is_ascii_digit())
                                .collect::<String>()
                                .get(0..4)
                                .and_then(|y| y.parse::<i32>().ok());
                        }
                        // Description/Summary (330 $a)
                        ("330", "a") => description = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.ends_with("datafield") {
                    in_datafield = false;
                    current_tag.clear();
                } else if name.ends_with("subfield") {
                    in_subfield = false;
                    current_code.clear();
                } else if name.ends_with("controlfield") {
                    in_controlfield = false;
                    controlfield_tag.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    if title.is_empty() {
        return Ok(None);
    }

    // Build author from structured fields if not found in 200$f
    if author.is_none() {
        if let (Some(surname), Some(firstname)) = (&author_surname, &author_firstname) {
            author = Some(format!("{} {}", firstname, surname));
        } else if let Some(surname) = author_surname {
            author = Some(surname);
        }
    }

    // Build cover URL from ARK ID (not always available)
    let cover_url = ark_id.as_ref().and_then(|ark| {
        // BNF cover service - may not work for all books
        Some(format!(
            "https://catalogue.bnf.fr/couverture?&appName=NE&idArk={}&couession=1",
            ark.trim_start_matches("ark:/12148/")
        ))
    });

    let book = BnfBook {
        title,
        author,
        publisher,
        publication_year: year,
        isbn: Some(clean_isbn.clone()),
        cover_url,
        bnf_uri: ark_id
            .map(|a| format!("https://catalogue.bnf.fr/{}", a))
            .unwrap_or_default(),
        description,
    };

    // Store in cache
    if let Ok(mut cache) = BNF_SRU_CACHE.try_lock() {
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, entry| entry.created_at.elapsed() < CACHE_TTL);
        }
        cache.insert(
            format!("isbn:{}", clean_isbn),
            CacheEntry {
                data: vec![book.clone()],
                created_at: Instant::now(),
            },
        );
    }

    Ok(Some(book))
}

/// Search books by title/author on catalogue.bnf.fr (SRU API)
/// Returns up to 20 results for French books
pub async fn search_bnf_sru(
    query: &str,
    title: Option<&str>,
    author: Option<&str>,
) -> Result<Vec<BnfBook>, String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    // Build SRU query for cache key
    let mut query_parts = Vec::new();

    if let Some(t) = title {
        if !t.trim().is_empty() {
            query_parts.push(format!("bib.title adj \"{}\"", t.replace('"', " ")));
        }
    }

    if let Some(a) = author {
        if !a.trim().is_empty() {
            query_parts.push(format!("bib.author adj \"{}\"", a.replace('"', " ")));
        }
    }

    // If no specific fields, use general query
    if query_parts.is_empty() && !query.trim().is_empty() {
        query_parts.push(format!("bib.anywhere adj \"{}\"", query.replace('"', " ")));
    }

    if query_parts.is_empty() {
        return Ok(Vec::new());
    }

    let sru_query = query_parts.join(" and ");
    let cache_key = format!("search:{}", sru_query.to_lowercase());

    // Check cache first
    if let Ok(cache) = BNF_SRU_CACHE.try_lock()
        && let Some(entry) = cache.get(&cache_key)
        && entry.created_at.elapsed() < CACHE_TTL
    {
        return Ok(entry.data.clone());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let encoded_query = urlencoding::encode(&sru_query);

    let url = format!(
        "https://catalogue.bnf.fr/api/SRU?version=1.2&operation=searchRetrieve&query={}&maximumRecords=20&recordSchema=unimarcxchange",
        encoded_query
    );

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("BNF SRU search failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("BNF SRU returned error: {}", response.status()));
    }

    let xml = response
        .text()
        .await
        .map_err(|e| format!("Failed to read BNF SRU response: {}", e))?;

    // Check if any records found
    if xml.contains("<srw:numberOfRecords>0</srw:numberOfRecords>") {
        return Ok(Vec::new());
    }

    // Parse multiple MARC records
    let mut books = Vec::new();
    let mut reader = Reader::from_str(&xml);
    reader.trim_text(true);

    let mut current_book = BnfBookBuilder::default();
    let mut buf = Vec::new();
    let mut current_tag = String::new();
    let mut current_code = String::new();
    let mut in_datafield = false;
    let mut in_subfield = false;
    let mut in_controlfield = false;
    let mut in_record = false;
    let mut controlfield_tag = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                if name.ends_with("record") {
                    in_record = true;
                } else if name.ends_with("datafield") && in_record {
                    in_datafield = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"tag" {
                            current_tag = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                } else if name.ends_with("subfield") && in_datafield {
                    in_subfield = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"code" {
                            current_code = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                } else if name.ends_with("controlfield") && in_record {
                    in_controlfield = true;
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"tag" {
                            controlfield_tag = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();

                if in_controlfield && controlfield_tag == "003" {
                    if let Some(ark_start) = text.find("ark:/12148/") {
                        current_book.ark_id = Some(text[ark_start..].to_string());
                    }
                }

                if in_subfield {
                    match (current_tag.as_str(), current_code.as_str()) {
                        ("200", "a") => current_book.title = text,
                        ("200", "f") if current_book.author.is_none() => {
                            current_book.author = Some(text)
                        }
                        ("700", "a") | ("701", "a") => current_book.author_surname = Some(text),
                        ("700", "b") | ("701", "b") => current_book.author_firstname = Some(text),
                        ("214", "c") | ("210", "c") => current_book.publisher = Some(text),
                        ("214", "d") | ("210", "d") => {
                            current_book.year = text
                                .chars()
                                .filter(|c| c.is_ascii_digit())
                                .collect::<String>()
                                .get(0..4)
                                .and_then(|y| y.parse::<i32>().ok());
                        }
                        ("010", "a") | ("073", "a") => {
                            // ISBN
                            let isbn = text.replace('-', "");
                            if isbn.len() == 13 || isbn.len() == 10 {
                                current_book.isbn = Some(isbn);
                            }
                        }
                        ("330", "a") => current_book.description = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name.ends_with("record") {
                    in_record = false;
                    // Build and save book if it has a title
                    let book = std::mem::take(&mut current_book);
                    if !book.title.is_empty() {
                        if let Some(b) = book.build() {
                            books.push(b);
                        }
                    }
                } else if name.ends_with("datafield") {
                    in_datafield = false;
                    current_tag.clear();
                } else if name.ends_with("subfield") {
                    in_subfield = false;
                    current_code.clear();
                } else if name.ends_with("controlfield") {
                    in_controlfield = false;
                    controlfield_tag.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    // Store in cache
    if let Ok(mut cache) = BNF_SRU_CACHE.try_lock() {
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, entry| entry.created_at.elapsed() < CACHE_TTL);
        }
        cache.insert(
            cache_key,
            CacheEntry {
                data: books.clone(),
                created_at: Instant::now(),
            },
        );
    }

    Ok(books)
}

/// Helper struct for building BnfBook from MARC fields
#[derive(Default)]
struct BnfBookBuilder {
    title: String,
    author: Option<String>,
    author_surname: Option<String>,
    author_firstname: Option<String>,
    publisher: Option<String>,
    year: Option<i32>,
    isbn: Option<String>,
    description: Option<String>,
    ark_id: Option<String>,
}

impl BnfBookBuilder {
    fn build(self) -> Option<BnfBook> {
        if self.title.is_empty() {
            return None;
        }

        let author = self
            .author
            .or_else(|| match (&self.author_firstname, &self.author_surname) {
                (Some(f), Some(s)) => Some(format!("{} {}", f, s)),
                (None, Some(s)) => Some(s.clone()),
                _ => None,
            });

        let cover_url = self.ark_id.as_ref().map(|ark| {
            format!(
                "https://catalogue.bnf.fr/couverture?&appName=NE&idArk={}&couession=1",
                ark.trim_start_matches("ark:/12148/")
            )
        });

        Some(BnfBook {
            title: self.title,
            author,
            publisher: self.publisher,
            publication_year: self.year,
            isbn: self.isbn,
            cover_url,
            bnf_uri: self
                .ark_id
                .map(|a| format!("https://catalogue.bnf.fr/{}", a))
                .unwrap_or_default(),
            description: self.description,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_search_bnf() {
        let results = search_bnf("Victor Hugo").await;
        if let Err(e) = &results {
            println!("BNF Search failed: {}", e);
        }
        assert!(results.is_ok(), "BNF search failed: {:?}", results.err());
    }

    #[tokio::test]
    async fn test_lookup_bnf_sru() {
        // ISBN not found in SPARQL but available in SRU
        let result = lookup_bnf_sru("9782226468345").await;
        assert!(result.is_ok(), "BNF SRU lookup failed: {:?}", result.err());
        let book = result.unwrap();
        assert!(book.is_some(), "Book should be found via BNF SRU");
        let book = book.unwrap();
        println!("Found: {} by {:?}", book.title, book.author);
        assert!(!book.title.is_empty(), "Title should not be empty");
    }

    #[tokio::test]
    async fn test_search_bnf_sru() {
        // Search by title
        let result = search_bnf_sru("", Some("Geronimo Stilton"), None).await;
        assert!(result.is_ok(), "BNF SRU search failed: {:?}", result.err());
        let books = result.unwrap();
        println!("Found {} books for 'Geronimo Stilton'", books.len());
        assert!(!books.is_empty(), "Should find books");
        // Check first result
        let first = &books[0];
        println!("First result: {} by {:?}", first.title, first.author);
    }
}
