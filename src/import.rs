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

pub fn parse_goodreads_csv(content: &[u8]) -> Result<Vec<CreateBookRequest>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(content);

    let mut books = Vec::new();

    for result in rdr.deserialize() {
        let record: GoodreadsBook = result.map_err(|e| format!("CSV parse error: {}", e))?;

        // Prefer ISBN13, fallback to ISBN, clean up formatting
        let isbn = record
            .isbn13
            .or(record.isbn)
            .map(|s| s.replace("=", "").replace("\"", "").trim().to_string())
            .filter(|s| !s.is_empty());

        books.push(CreateBookRequest {
            title: record.title,
            isbn,
            publisher: record.publisher,
            publication_year: record.year_published,
        });
    }

    Ok(books)
}
