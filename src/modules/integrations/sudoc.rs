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
    pub summary: Option<String>,
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
    // NOT trim_text(true): a subfield's text arrives in fragments whenever it
    // contains an entity, and trimming each fragment eats the spaces on
    // either side of it ("Orgueil &amp; prejuges" would become
    // "Orgueil&prejuges"). The accumulated value is trimmed once instead.
    reader.config_mut().trim_text(false);

    let mut title = String::new();
    let mut publisher = None;
    let mut year = None;
    let mut dewey = None;
    let mut subjects = Vec::new();
    let mut summary = None;

    // UNIMARC author candidates, in priority order.
    // 700 = main author, 701 = alternative, 702 = secondary (translator/editor).
    // 200 $f is the free-text statement of responsibility and must only be a
    // last-resort fallback (it contains sentences like "présenté par X", not
    // a clean author name).
    let mut author_700: (Option<String>, Option<String>) = (None, None);
    let mut author_701: (Option<String>, Option<String>) = (None, None);
    let mut author_702: (Option<String>, Option<String>) = (None, None);
    let mut responsibility_200f: Option<String> = None;

    let mut buf = Vec::new();
    let mut current_tag = String::new();
    let mut current_code = String::new();
    // A subfield's value is ACCUMULATED and dispatched once, at its closing
    // tag. quick-xml reports an entity reference as its own event, so
    // "L&apos;etranger" arrives as Text("L"), GeneralRef("apos"),
    // Text("etranger"); a dispatch that assigned on every text event kept
    // only the last fragment and stored the book as "etranger".
    let mut current_text = String::new();
    let mut in_subfield = false;

    // Simple parser state machine
    // Note: SUDOC XML is MARCXML-like but specific (UNIMARC).
    // We look for specific datafields.
    // 200 $a = Title
    // 200 $f = Statement of responsibility (free text, fallback only)
    // 210 $c = Publisher
    // 210 $d = Year
    // 330 $a = Summary / abstract (4ème de couverture)
    // 676 $a = Dewey
    // 606 $a = Subject (RAMEAU)
    // 700/701/702 $a = Author surname, $b = Author firstname

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
                    current_text.clear();
                    in_subfield = true;
                }
            }
            Ok(Event::Text(e)) if in_subfield => {
                current_text.push_str(&super::xml_text_content(&e));
            }
            // The other half of a fragmented value: `&amp;`, `&apos;` and
            // `&#233;` are all reported here rather than inside the text.
            Ok(Event::GeneralRef(e)) => {
                if in_subfield && let Some(resolved) = super::xml_entity_text(&e) {
                    current_text.push_str(&resolved);
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"subfield" => {
                in_subfield = false;
                let text = current_text.trim().to_string();
                match (current_tag.as_str(), current_code.as_str()) {
                    ("200", "a") => title = text,
                    ("200", "f") if responsibility_200f.is_none() => {
                        responsibility_200f = Some(text)
                    }
                    ("700", "a") if author_700.1.is_none() => author_700.1 = Some(text),
                    ("700", "b") if author_700.0.is_none() => author_700.0 = Some(text),
                    ("701", "a") if author_701.1.is_none() => author_701.1 = Some(text),
                    ("701", "b") if author_701.0.is_none() => author_701.0 = Some(text),
                    ("702", "a") if author_702.1.is_none() => author_702.1 = Some(text),
                    ("702", "b") if author_702.0.is_none() => author_702.0 = Some(text),
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
                    ("330", "a") => summary = Some(text),
                    ("676", "a") => dewey = Some(text),
                    ("606", "a") => subjects.push(text),
                    _ => {}
                }
                current_code.clear();
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).unwrap_or("");
                if name == "datafield" {
                    current_tag.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML Parse Error: {}", e)),
            _ => (),
        }
        buf.clear();
    }

    let author =
        super::unimarc::compose_author(author_700, author_701, author_702, responsibility_200f);

    Ok(SudocBook {
        title,
        author,
        publisher,
        publication_year: year,
        dewey,
        subjects,
        summary,
        ppn: ppn.to_string(),
        raw_data: Some(xml.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real SUDOC UNIMARC record for ISBN 9782367321257
    /// (Le voyage de Magellan, Pigafetta / Castro).
    /// The record has both `700` (Pigafetta, primary author) and `200 $f`
    /// ("transcrite, présentée & annotée par Xavier de Castro" — a free-text
    /// statement of responsibility). Author must come from `700`, not `200 $f`.
    const SUDOC_PIGAFETTA_FIXTURE: &str =
        include_str!("../../../tests/fixtures/sudoc_9782367321257.xml");

    /// The second half of the same trap, and the one a naive fix walks into.
    ///
    /// The reader used to trim EVERY text event, so the fragments around an
    /// entity lost the spaces that separated them from it: accumulating
    /// "Orgueil " + "&" + " prejuges" under that setting yields
    /// "Orgueil&prejuges". Interior spacing survives, and the trim happens
    /// once on the finished value.
    #[test]
    fn keeps_the_spaces_around_an_entity() {
        let xml = r##"<?xml version="1.0" encoding="UTF-8"?>
<record>
  <datafield tag="200" ind1="1" ind2=" ">
    <subfield code="a">  Orgueil &amp; pr&#233;jug&#233;s  </subfield>
  </datafield>
</record>"##;

        let book = parse_sudoc_xml(xml, "1").expect("record parses");

        assert_eq!(book.title, "Orgueil & préjugés");
    }

    /// A title with an apostrophe, exactly as the SUDOC serves it.
    ///
    /// `L&apos;etranger` is an entity reference in the middle of a text run,
    /// and quick-xml 0.41 reports it as its own event: the parser sees
    /// Text("L"), a general reference, then Text("etranger"). A dispatch that
    /// ASSIGNS on every text event keeps only the last fragment, so the book
    /// is stored as "etranger". French titles are full of apostrophes, and
    /// the BnF serves `&amp;` the same way ("Orgueil &amp; prejuges").
    #[test]
    fn keeps_the_whole_title_when_it_contains_an_entity() {
        let xml = r##"<?xml version="1.0" encoding="UTF-8"?>
<record>
  <datafield tag="200" ind1="1" ind2=" ">
    <subfield code="a">L&apos;etranger</subfield>
  </datafield>
  <datafield tag="700" ind1="#" ind2="1">
    <subfield code="a">Camus</subfield>
    <subfield code="b">Albert</subfield>
  </datafield>
</record>"##;

        let book = parse_sudoc_xml(xml, "001896431").expect("record parses");

        assert_eq!(book.title, "L'etranger");
    }

    #[test]
    fn parses_author_from_700_not_200f() {
        let book = parse_sudoc_xml(SUDOC_PIGAFETTA_FIXTURE, "224415891").unwrap();
        assert_eq!(book.title, "Le voyage de Magellan");
        assert_eq!(
            book.author.as_deref(),
            Some("Antonio Pigafetta"),
            "Author must come from UNIMARC 700, not the 200$f responsibility statement",
        );
    }
}
