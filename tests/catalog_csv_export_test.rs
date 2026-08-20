#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
//! Integration tests for `GET /api/export/csv` (`api::export::export_catalog_csv`).
//!
//! This export is the human-readable inventory listing, not a backup: it is
//! never re-imported, so what matters is that a spreadsheet opens it intact
//! (BOM, `;` delimiter, quoted fields) and that possession is reported
//! correctly. Covers: the byte-order mark, escaping of a title carrying the
//! delimiter, a quote and a newline, the three `ownership_status` values, and
//! a book with no author.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use rust_lib_app::api;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use tower::util::ServiceExt; // for `oneshot`

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

async fn setup_test_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB")
}

/// Insert a book, returning its uuid.
async fn create_book(
    db: &DatabaseConnection,
    title: &str,
    owned: bool,
    reading_status: &str,
) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set(title.to_string()),
        owned: Set(owned),
        private: Set(false),
        reading_status: Set(reading_status.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book.insert(db).await.expect("insert book").id
}

/// Attach an author to a book, creating the author row.
async fn add_author(db: &DatabaseConnection, book_id: &str, name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let author = rust_lib_app::models::author::ActiveModel {
        name: Set(name.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let author = author.insert(db).await.expect("insert author");
    let link = rust_lib_app::models::book_authors::ActiveModel {
        book_id: Set(book_id.to_string()),
        author_id: Set(author.id),
    };
    link.insert(db).await.expect("link author");
}

/// Attach a tag to a book, creating the tag row.
async fn add_tag(db: &DatabaseConnection, book_id: &str, name: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let tag = rust_lib_app::models::tag::ActiveModel {
        name: Set(name.to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    let tag = tag.insert(db).await.expect("insert tag");
    let link = rust_lib_app::models::book_tags::ActiveModel {
        book_id: Set(book_id.to_string()),
        tag_id: Set(tag.id),
    };
    link.insert(db).await.expect("link tag");
}

/// Insert a copy of `book_id` with the given status. `is_temporary` is left
/// false on purpose: the borrowed derivation must not depend on it.
async fn create_copy(db: &DatabaseConnection, book_id: &str, status: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let copy = rust_lib_app::models::copy::ActiveModel {
        book_id: Set(book_id.to_string()),
        library_id: Set(1),
        status: Set(status.to_string()),
        is_temporary: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    copy.insert(db).await.expect("insert copy");
}

/// Call the endpoint and return (status, raw body bytes).
async fn fetch_csv(db: DatabaseConnection) -> (StatusCode, Vec<u8>) {
    let app = Router::new()
        .route(
            "/export/csv",
            axum::routing::get(api::export::export_catalog_csv),
        )
        .with_state(AppState::new(db));

    let req = Request::builder()
        .uri("/export/csv")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.expect("request");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, body.to_vec())
}

/// Body as text, byte-order mark stripped.
fn csv_text(body: &[u8]) -> String {
    assert_eq!(&body[..3], UTF8_BOM, "export must start with a UTF-8 BOM");
    String::from_utf8(body[3..].to_vec()).expect("valid UTF-8")
}

#[tokio::test]
async fn test_csv_export_has_bom_and_semicolon_header() {
    let db = setup_test_db().await;
    create_book(&db, "Les Misérables", true, "read").await;

    let (status, body) = fetch_csv(db).await;
    assert_eq!(status, StatusCode::OK);

    // The BOM is what makes Excel read the accented title as UTF-8.
    assert_eq!(&body[..3], UTF8_BOM);

    let text = csv_text(&body);
    let header = text.lines().next().expect("header line");
    // Column names are quoted like every other non-numeric field.
    assert_eq!(
        header,
        "\"title\";\"authors\";\"isbn\";\"publisher\";\"publication_year\";\
         \"language\";\"ownership_status\";\"reading_status\";\"user_rating\";\
         \"price\";\"tags\";\"added_at\""
    );
    assert!(text.contains("Les Misérables"));

    // Rows are terminated by a bare LF. The Flutter side rewrites the file and
    // locates rows by `\n`, so a switch to CRLF here would silently leave
    // stray `\r` characters behind.
    assert!(
        text.contains("\"added_at\"\n") && !text.contains("\"added_at\"\r\n"),
        "unexpected line terminator"
    );
}

#[tokio::test]
async fn test_csv_export_quotes_a_field_holding_a_comma() {
    let db = setup_test_db().await;
    // Two authors are joined by ", ". A `;`-delimited reader does not need the
    // quotes, but LibreOffice splits on a comma when its import dialog still
    // has that separator ticked, which put the second author in the ISBN
    // column.
    let book = create_book(&db, "Le Horla", true, "read").await;
    add_author(&db, &book, "Maupassant, Guy de").await;
    add_author(&db, &book, "Autre, Auteur").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);

    assert!(
        text.contains("\"Autre, Auteur, Maupassant, Guy de\""),
        "the authors cell must be quoted: {text}"
    );

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let record = reader.records().next().expect("row").expect("record");
    assert_eq!(record.len(), 12, "a comma split the row");
    assert_eq!(&record[1], "Autre, Auteur, Maupassant, Guy de");
}

#[tokio::test]
async fn test_csv_export_defuses_spreadsheet_formulas() {
    let db = setup_test_db().await;
    // Not a hypothetical: the title of a borrowed book is whatever the lending
    // peer sent, and metadata providers are not first-party either. Excel and
    // LibreOffice execute a cell starting with one of these, quoted or not.
    let equals = create_book(&db, "=HYPERLINK(\"http://evil\",\"click\")", true, "read").await;
    add_tag(&db, &equals, "@SUM(1+1)").await;
    create_book(&db, "+41 tours du monde", true, "read").await;
    let minus = create_book(&db, "-Le Horla", true, "read").await;
    add_author(&db, &minus, "=cmd|'/c calc'!A0").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let rows: Vec<csv::StringRecord> = reader.records().map(|r| r.expect("record")).collect();

    // Sorted by title: '+' then '-' then '='.
    let by_title = |needle: &str| -> csv::StringRecord {
        rows.iter()
            .find(|r| r[0].contains(needle))
            .unwrap_or_else(|| panic!("row {needle} missing from {text}"))
            .clone()
    };

    let row = by_title("HYPERLINK");
    assert!(
        row[0].starts_with('\''),
        "title still a formula: {}",
        &row[0]
    );
    assert!(
        row[10].starts_with('\''),
        "tag still a formula: {}",
        &row[10]
    );

    assert!(by_title("41 tours")[0].starts_with('\''));

    let row = by_title("Le Horla");
    assert!(
        row[0].starts_with('\''),
        "title still a formula: {}",
        &row[0]
    );
    assert!(
        row[1].starts_with('\''),
        "author still a formula: {}",
        &row[1]
    );
}

#[tokio::test]
async fn test_csv_export_leaves_a_plain_title_and_a_negative_price_alone() {
    let db = setup_test_db().await;
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Les Misérables".to_string()),
        owned: Set(true),
        private: Set(false),
        reading_status: Set("read".to_string()),
        // A negative number is not a formula. Defusing it would quote it and
        // cost the spreadsheet its numeric sort.
        price: Set(Some(-5.0)),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book.insert(&db).await.expect("insert book");

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);

    assert!(
        text.contains("\"Les Misérables\""),
        "title was altered: {text}"
    );
    assert!(
        !text.contains("'Les Misérables"),
        "title was defused: {text}"
    );
    assert!(text.contains(";-5;"), "negative price was defused: {text}");
}

#[tokio::test]
async fn test_csv_export_leaves_numbers_unquoted() {
    let db = setup_test_db().await;
    let now = chrono::Utc::now().to_rfc3339();
    let book = rust_lib_app::models::book::ActiveModel {
        title: Set("Numbers".to_string()),
        owned: Set(true),
        private: Set(false),
        reading_status: Set("read".to_string()),
        publication_year: Set(Some(1862)),
        user_rating: Set(Some(9)),
        price: Set(Some(12.5)),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    book.insert(&db).await.expect("insert book");

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);

    // Bare, so the spreadsheet sorts them as numbers and not as text.
    assert!(
        text.contains(";1862;"),
        "publication_year was quoted: {text}"
    );
    assert!(text.contains(";9;"), "user_rating was quoted: {text}");
    assert!(text.contains(";12.5;"), "price was quoted: {text}");
}

#[tokio::test]
async fn test_csv_export_headers_advertise_a_csv_attachment() {
    let db = setup_test_db().await;
    create_book(&db, "A Book", true, "to_read").await;

    let app = Router::new()
        .route(
            "/export/csv",
            axum::routing::get(api::export::export_catalog_csv),
        )
        .with_state(AppState::new(db));
    let req = Request::builder()
        .uri("/export/csv")
        .method("GET")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.expect("request");

    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(content_type, "text/csv; charset=utf-8");

    let disposition = response
        .headers()
        .get(axum::http::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        disposition.starts_with("attachment; filename=\"bibliogenius_catalogue_"),
        "unexpected disposition: {disposition}"
    );
    assert!(disposition.ends_with(".csv\""));
}

#[tokio::test]
async fn test_csv_export_escapes_delimiter_quote_and_newline() {
    let db = setup_test_db().await;
    // Every character that can break a naive CSV writer, in one title.
    let nasty = "Ainsi; parla \"Zarathoustra\"\nou presque";
    create_book(&db, nasty, true, "read").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);

    // The whole field is quoted and the inner quotes doubled, so the `;` and
    // the newline stay inside one cell.
    assert!(
        text.contains("\"Ainsi; parla \"\"Zarathoustra\"\"\nou presque\""),
        "title was not escaped: {text}"
    );

    // Re-read with the same dialect: one data row, and the title survives byte
    // for byte.
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let records: Vec<csv::StringRecord> = reader.records().map(|r| r.expect("record")).collect();
    assert_eq!(records.len(), 1, "escaping split the row");
    assert_eq!(&records[0][0], nasty);
}

#[tokio::test]
async fn test_csv_export_derives_ownership_status() {
    let db = setup_test_db().await;

    // owned: the book flag is set, whatever the copies say.
    let owned = create_book(&db, "A owned", true, "read").await;
    create_copy(&db, &owned, "available").await;

    // borrowed: not owned, but an active copy with status 'borrowed'. Note
    // is_temporary = false (a contact loan, ADR-034) to prove the derivation
    // keys on status alone.
    let borrowed = create_book(&db, "B borrowed", false, "reading").await;
    create_copy(&db, &borrowed, "borrowed").await;

    // wishlist: not owned, no borrowed copy.
    create_book(&db, "C wishlist", false, "wanting").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let records: Vec<csv::StringRecord> = reader.records().map(|r| r.expect("record")).collect();
    assert_eq!(records.len(), 3);

    // Rows come back sorted by title, hence the A/B/C prefixes above.
    assert_eq!(&records[0][0], "A owned");
    assert_eq!(&records[0][6], "owned");
    assert_eq!(&records[1][0], "B borrowed");
    assert_eq!(&records[1][6], "borrowed");
    assert_eq!(&records[2][0], "C wishlist");
    assert_eq!(&records[2][6], "wishlist");

    // reading_status stays the raw stored value: no `borrowed`/`lent` overlay.
    assert_eq!(&records[1][7], "reading");
}

#[tokio::test]
async fn test_csv_export_owned_wins_over_a_borrowed_copy() {
    let db = setup_test_db().await;
    // Owning a title and borrowing another copy of it at the same time is
    // legal; the inventory must still call it ours.
    let book = create_book(&db, "Double", true, "read").await;
    create_copy(&db, &book, "available").await;
    create_copy(&db, &book, "borrowed").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let record = reader.records().next().expect("row").expect("record");
    assert_eq!(&record[6], "owned");
}

#[tokio::test]
async fn test_csv_export_book_without_author_keeps_its_columns() {
    let db = setup_test_db().await;
    let orphan = create_book(&db, "Anonyme", true, "to_read").await;
    add_tag(&db, &orphan, "poésie").await;

    let with_authors = create_book(&db, "Zola et Cie", true, "read").await;
    add_author(&db, &with_authors, "Zola, Émile").await;
    add_author(&db, &with_authors, "Autre, Auteur").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let records: Vec<csv::StringRecord> = reader.records().map(|r| r.expect("record")).collect();
    assert_eq!(records.len(), 2);

    // Authorless book: an empty cell, not a missing column and not a shifted row.
    assert_eq!(&records[0][0], "Anonyme");
    assert_eq!(&records[0][1], "");
    assert_eq!(records[0].len(), 12);
    assert_eq!(&records[0][10], "poésie");

    // Several authors are joined by ", " in a single quoted cell, name-sorted
    // so two exports of the same library match.
    assert_eq!(&records[1][1], "Autre, Auteur, Zola, Émile");
    assert_eq!(records[1].len(), 12);
}

#[tokio::test]
async fn test_csv_export_added_at_is_a_bare_date() {
    let db = setup_test_db().await;
    create_book(&db, "Dated", true, "read").await;

    let (_, body) = fetch_csv(db).await;
    let text = csv_text(&body);
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes());
    let record = reader.records().next().expect("row").expect("record");
    let added_at = &record[11];
    assert_eq!(added_at.len(), 10, "expected YYYY-MM-DD, got {added_at}");
    assert!(!added_at.contains('T'));
}

#[tokio::test]
async fn test_csv_export_empty_library_still_has_a_header() {
    let db = setup_test_db().await;

    let (status, body) = fetch_csv(db).await;
    assert_eq!(status, StatusCode::OK);
    let text = csv_text(&body);
    assert!(
        text.starts_with("\"title\";\"authors\";\"isbn\";"),
        "got: {text}"
    );
    assert_eq!(text.lines().count(), 1);
}
