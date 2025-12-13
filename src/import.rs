use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct CreateBookRequest {
    pub title: String,
    pub isbn: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct GoodreadsBook {
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Author")]
    _author: String,
    #[serde(rename = "ISBN13")]
    isbn13: Option<String>,
    #[serde(rename = "ISBN")]
    isbn: Option<String>,
    #[serde(rename = "Publisher")]
    publisher: Option<String>,
    #[serde(rename = "Year Published")]
    year_published: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct LibraryThingBook {
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Primary Author")]
    _author: String,
    #[serde(rename = "ISBN")]
    isbn: Option<String>,
    #[serde(rename = "Publication")]
    publication: Option<String>, // Often contains publisher and year
    #[serde(rename = "Date")]
    date: Option<String>, // Year
}

#[derive(Debug, Deserialize)]
struct BabelioBook {
    #[serde(rename = "Titre")]
    title: String,
    #[serde(rename = "Auteur")]
    _author: String,
    #[serde(rename = "EAN")]
    ean: Option<String>, // ISBN 13
    #[serde(rename = "Editeur")]
    editeur: Option<String>,
    #[serde(rename = "Date de publication")]
    date_publication: Option<String>,
}

pub fn parse_import_file(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    // 1. Try to detect format based on headers
    let content_str = String::from_utf8_lossy(content);
    let first_line = content_str.lines().next().unwrap_or("").trim();

    if first_line.contains("ISBN13") && first_line.contains("Title") {
        return parse_goodreads_csv(content);
    } else if first_line.contains("Primary Author") && first_line.contains("ISBN") {
        return parse_librarything_csv(content);
    } else if first_line.contains("Titre") && first_line.contains("EAN") {
        return parse_babelio_csv(content);
    } else if first_line.contains("Item URL") && first_line.contains("Edition ISBN-13") {
        return parse_inventaire_csv(content);
    } else if content_str.trim_start().starts_with('{') && content_str.contains("\"items\"") {
        return parse_inventaire_json(content);
    }

    // 2. Fallback: Treat as raw ISBN list if it looks like a list of numbers
    // Check if first few lines look like ISBNs (10 or 13 digits)
    let is_isbn_list = content_str.lines().take(5).all(|line| {
        line.trim()
            .chars()
            .all(|c| c.is_numeric() || c == '-' || c == 'X')
    });

    if is_isbn_list {
        return parse_isbn_list(content);
    }

    Err("Unknown file format. Supported: Goodreads, LibraryThing, Babelio, ISBN List".to_string())
}

fn parse_goodreads_csv(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(content);

    let mut books = Vec::new();

    for result in rdr.deserialize() {
        let record: GoodreadsBook = result.map_err(|e| format!("CSV parse error: {}", e))?;
        let isbn = clean_isbn(record.isbn13.or(record.isbn));

        books.push(CreateBookRequest {
            title: record.title,
            isbn,
            publisher: record.publisher,
            publication_year: record.year_published,
        });
    }
    Ok(books)
}

fn parse_librarything_csv(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    // LibraryThing CSV is UTF-16LE encoded sometimes, but we assume UTF-8 for now or handled by upload
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(content);

    let mut books = Vec::new();

    for result in rdr.deserialize() {
        // LibraryThing export is notoriously messy, often tab-delimited or comma
        // We assume standard CSV for now
        let record: LibraryThingBook = result.map_err(|e| format!("CSV parse error: {}", e))?;
        let isbn = clean_isbn(record.isbn);

        let year = record.date.and_then(|d| d.parse::<i32>().ok());

        books.push(CreateBookRequest {
            title: record.title,
            isbn,
            publisher: record.publication, // Rough mapping
            publication_year: year,
        });
    }
    Ok(books)
}

fn parse_babelio_csv(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .delimiter(b';') // Babelio uses semicolons
        .from_reader(content);

    let mut books = Vec::new();

    for result in rdr.deserialize() {
        let record: BabelioBook = result.map_err(|e| format!("CSV parse error: {}", e))?;
        let isbn = clean_isbn(record.ean);

        // Extract year from "01/01/2020"
        let year = record
            .date_publication
            .and_then(|d| d.split('/').next_back().map(|y| y.to_string()))
            .and_then(|y| y.parse::<i32>().ok());

        books.push(CreateBookRequest {
            title: record.title,
            isbn,
            publisher: record.editeur,
            publication_year: year,
        });
    }
    Ok(books)
}

fn parse_isbn_list(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    let content_str = String::from_utf8_lossy(content);
    let mut books = Vec::new();

    for line in content_str.lines() {
        let isbn = line.trim().replace("-", "");
        if !isbn.is_empty() {
            books.push(CreateBookRequest {
                title: format!("Imported ISBN {}", isbn), // Placeholder title
                isbn: Some(isbn),
                publisher: None,
                publication_year: None,
            });
        }
    }
    Ok(books)
}

fn clean_isbn(isbn: Option<String>) -> Option<String> {
    isbn.map(|s| {
        s.replace("=", "")
            .replace("\"", "")
            .replace("-", "")
            .trim()
            .to_string()
    })
    .filter(|s| !s.is_empty())
}

#[derive(Debug, Deserialize)]
struct InventaireBook {
    #[serde(rename = "Edition title")]
    title: String,
    #[serde(rename = "Edition ISBN-13")]
    isbn13: Option<String>,
    #[serde(rename = "Edition ISBN-10")]
    isbn10: Option<String>,
    #[serde(rename = "Publisher label")]
    publisher: Option<String>,
    #[serde(rename = "Edition publication date")]
    publication_date: Option<String>,
}

fn parse_inventaire_csv(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(content);

    let mut books = Vec::new();

    for result in rdr.deserialize() {
        let record: InventaireBook = result.map_err(|e| format!("CSV parse error: {}", e))?;
        let isbn = clean_isbn(record.isbn13.or(record.isbn10));

        // Parse year from "YYYY-MM-DD" or just "YYYY"
        let year = record
            .publication_date
            .and_then(|d| d.split('-').next().map(|y| y.to_string()))
            .and_then(|y| y.parse::<i32>().ok());

        books.push(CreateBookRequest {
            title: record.title,
            isbn,
            publisher: record.publisher,
            publication_year: year,
        });
    }
    Ok(books)
}

fn parse_inventaire_json(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    let content_str = String::from_utf8_lossy(content);

    #[derive(Deserialize)]
    struct Root {
        items: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        entity: String,
        snapshot: Option<Snapshot>,
    }
    #[derive(Deserialize)]
    struct Snapshot {
        #[serde(rename = "entity:title")]
        title: Option<String>,
        #[serde(rename = "entity:authors")]
        _authors: Option<serde_json::Value>,
    }

    let root: Root =
        serde_json::from_str(&content_str).map_err(|e| format!("JSON parse error: {}", e))?;

    let mut books = Vec::new();

    for item in root.items {
        if let Some(snapshot) = item.snapshot {
            let title = snapshot
                .title
                .unwrap_or_else(|| "Unknown Title".to_string());

            // Extract ISBN from "entity":"isbn:978..."
            let isbn = if item.entity.starts_with("isbn:") {
                Some(item.entity.replace("isbn:", ""))
            } else {
                None
            };

            books.push(CreateBookRequest {
                title,
                isbn: clean_isbn(isbn),
                publisher: None,
                publication_year: None,
            });
        }
    }
    Ok(books)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_inventaire_csv() {
        let csv_content = r#"Item URL,Item details,Item notes,Item visibility,Item transaction,Item created,Shelves,Edition URL,Edition ISBN-13,Edition ISBN-10,Edition title,Edition subtitle,Edition publication date,Edition cover,Edition number of pages,Edition language,Works URLs,Works labels,Original language,Works Series ordinals,Authors URLs,Authors labels,Translators labels,Translators URLs,Series URLs,Series labels,Genres URLs,Genres labels,Subjects URLs,Subjects labels,Publisher URLs,Publisher label
https://inventaire.io/items/123,,,"friends,groups",inventorying,2025-12-05T06:10:16.091Z,,https://inventaire.io/entity/isbn:9782264024848,978-2-264-02484-8,2-264-02484-4,Martin Eden,,1999-09-12,https://inventaire.io/img/entities/879f475f9346653da1811850cc881ee153c8193d,,français,https://inventaire.io/entity/wd:Q1317839,Martin Eden,anglais,,https://inventaire.io/entity/wd:Q45765,Jack London,,,,,https://inventaire.io/entity/wd:Q783459,Künstlerroman,,,,"#;

        let result = parse_import_file(csv_content.as_bytes()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].title, "Martin Eden");
        assert_eq!(result[0].isbn, Some("9782264024848".to_string()));
        assert_eq!(result[0].publication_year, Some(1999));
    }

    #[test]
    fn test_parse_inventaire_json() {
        let json_content = r#"{
            "items": [
                {
                    "entity": "isbn:9782264024848",
                    "snapshot": {
                        "entity:title": "Martin Eden",
                        "entity:authors": "Jack London"
                    }
                },
                {
                    "entity": "isbn:9782330124298",
                    "snapshot": {
                        "entity:title": "Mille petits riens",
                        "entity:authors": ["Jodi Picoult"]
                    }
                }
            ]
        }"#;

        let result = parse_import_file(json_content.as_bytes()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].title, "Martin Eden");
        assert_eq!(result[0].isbn, Some("9782264024848".to_string()));

        assert_eq!(result[1].title, "Mille petits riens");
        assert_eq!(result[1].isbn, Some("9782330124298".to_string()));
    }
}
