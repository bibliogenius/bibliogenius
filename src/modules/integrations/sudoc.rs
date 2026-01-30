use once_cell::sync::Lazy;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cache entry for SUDOC queries
struct CacheEntry {
    data: SudocBook,
    created_at: Instant,
}

static SUDOC_CACHE: Lazy<Mutex<HashMap<String, CacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour
const MAX_CACHE_ENTRIES: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SudocBook {
    pub title: String,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub dewey: Option<String>,
    pub subjects: Vec<String>,
    pub ppn: String,
    pub raw_data: Option<String>,
}

pub async fn fetch_by_isbn(isbn: &str) -> Result<SudocBook, String> {
    let clean_isbn = isbn.replace('-', "");

    // Check cache first
    if let Ok(cache) = SUDOC_CACHE.try_lock()
        && let Some(entry) = cache.get(&clean_isbn)
        && entry.created_at.elapsed() < CACHE_TTL
    {
        return Ok(entry.data.clone());
    }

    // 1. Get PPN from ISBN
    // URL: https://www.sudoc.fr/services/isbn2ppn/{isbn}
    // Response is JSON: {"sudoc":{"query":{"isbn":"..."},"result":[{"ppn":"..."}]}}

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let ppn_url = format!("https://www.sudoc.fr/services/isbn2ppn/{}", clean_isbn);

    let ppn_res = client
        .get(&ppn_url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !ppn_res.status().is_success() {
        return Err(format!("SUDOC API error: {}", ppn_res.status()));
    }

    let ppn_json: serde_json::Value = ppn_res.json().await.map_err(|e| e.to_string())?;
    tracing::debug!("SUDOC JSON: {:?}", ppn_json);

    // Extract PPN - handle both single result (object) and multiple results (array)
    let result = &ppn_json["sudoc"]["query"]["result"];
    let ppn = if result.is_array() {
        // Multiple results: take the first one
        result[0]["ppn"].as_str()
    } else {
        // Single result: direct object
        result["ppn"].as_str()
    }
    .ok_or("No PPN found for this ISBN")?
    .to_string();

    // 2. Fetch XML Record
    // URL: https://www.sudoc.fr/{ppn}.xml
    let xml_url = format!("https://www.sudoc.fr/{}.xml", ppn);
    let xml_res = client
        .get(&xml_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let xml_content = xml_res.text().await.map_err(|e| e.to_string())?;

    // 3. Parse XML
    let book = parse_sudoc_xml(&xml_content, &ppn)?;

    // Store in cache
    if let Ok(mut cache) = SUDOC_CACHE.try_lock() {
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.retain(|_, entry| entry.created_at.elapsed() < CACHE_TTL);
        }
        cache.insert(
            clean_isbn.clone(),
            CacheEntry {
                data: book.clone(),
                created_at: Instant::now(),
            },
        );
    }

    Ok(book)
}

fn parse_sudoc_xml(xml: &str, ppn: &str) -> Result<SudocBook, String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut title = String::new();
    let mut author = None;
    let mut publisher = None;
    let mut year = None;
    let mut dewey = None;
    let mut subjects = Vec::new();

    let mut buf = Vec::new();
    let mut current_tag = String::new();
    let mut current_code = String::new();

    // Simple parser state machine
    // Note: SUDOC XML is MARCXML-like but specific.
    // We look for specific datafields.
    // 200 $a = Title
    // 200 $f = Author
    // 210 $c = Publisher
    // 210 $d = Year
    // 676 $a = Dewey
    // 606 $a = Subject (RAMEAU)

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                if name == "datafield" {
                    // Extract 'tag' attribute
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == b"tag" {
                            current_tag = String::from_utf8_lossy(&a.value).to_string();
                        }
                    }
                } else if name == "subfield" {
                    // Extract 'code' attribute
                    for a in e.attributes().flatten() {
                        if a.key.as_ref() == b"code" {
                            current_code = String::from_utf8_lossy(&a.value).to_string();
                        }
                    }
                }
            }
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default().to_string();

                match (current_tag.as_str(), current_code.as_str()) {
                    ("200", "a") => title = text,
                    ("200", "f") => author = Some(text),
                    ("210", "c") => publisher = Some(text),
                    ("210", "d") => {
                        // Extract year (first 4 digits)
                        if let Ok(y) = text
                            .chars()
                            .filter(|c| c.is_ascii_digit())
                            .take(4)
                            .collect::<String>()
                            .parse::<i32>()
                        {
                            year = Some(y);
                        }
                    }
                    ("676", "a") => dewey = Some(text),
                    ("606", "a") => subjects.push(text),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                if name == "datafield" {
                    current_tag.clear();
                } else if name == "subfield" {
                    current_code.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML Parse Error: {}", e)),
            _ => (),
        }
        buf.clear();
    }

    Ok(SudocBook {
        title,
        author,
        publisher,
        publication_year: year,
        dewey,
        subjects,
        ppn: ppn.to_string(),
        raw_data: Some(xml.to_string()),
    })
}
