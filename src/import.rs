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
            .and_then(|d| d.split('/').last().map(|y| y.to_string()))
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
    isbn.map(|s| s.replace("=", "").replace("\"", "").trim().to_string())
        .filter(|s| !s.is_empty())
}
