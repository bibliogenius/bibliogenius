//! Book Service - Pure business logic without HTTP layer
//!
//! This module contains all book-related operations extracted from Axum handlers.
//! It can be called directly via FFI or through HTTP handlers.
#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
use std::collections::HashMap;

use crate::models::Book;
use crate::models::book::{ActiveModel as BookActiveModel, Entity as BookEntity};

/// Filter parameters for listing books
#[derive(Debug, Default, Clone)]
pub struct BookFilter {
    pub status: Option<String>,
    pub author: Option<String>,
    pub title: Option<String>,
    pub tag: Option<String>,
}

/// Tag with count for UI display
#[derive(Debug, Clone)]
pub struct TagDto {
    pub name: String,
    pub count: usize,
}

/// A cover candidate from an external source, for the multi-cover picker.
#[derive(Debug, Clone)]
pub struct CoverCandidate {
    pub url: String,
    pub source: String,
    /// Edition language code (e.g. "fr", "en"), used for sorting by relevance.
    pub language: Option<String>,
}

/// Error type for service operations
#[derive(Debug)]
pub enum ServiceError {
    Database(String),
    NotFound,
    InvalidInput(String),
}

impl From<sea_orm::DbErr> for ServiceError {
    fn from(e: sea_orm::DbErr) -> Self {
        ServiceError::Database(e.to_string())
    }
}

/// Populate `Book.available_copies` from the `copies` table for a batch of
/// books. Must run before serving any `/api/books*` response so peers can
/// tell which books are actually borrowable — without it, the iPhone-side
/// peer carousel filter receives `None` and can't drop books whose copies
/// are all on loan.
///
/// A single batch `IN (...)` query keeps this O(1) round-trips regardless
/// of the book count.
pub async fn populate_available_copies(
    db: &DatabaseConnection,
    books: &mut [Book],
) -> Result<(), sea_orm::DbErr> {
    let book_ids: Vec<i32> = books.iter().filter_map(|b| b.id).collect();
    if book_ids.is_empty() {
        return Ok(());
    }
    let copies = crate::models::copy::Entity::find()
        .filter(crate::models::copy::Column::BookId.is_in(book_ids))
        .all(db)
        .await?;

    let mut available_map: HashMap<i32, i32> = HashMap::new();
    for c in &copies {
        if c.status == "available" {
            *available_map.entry(c.book_id).or_insert(0) += 1;
        }
    }
    for book in books.iter_mut() {
        let id = book.id.unwrap_or(0);
        book.available_copies = Some(*available_map.get(&id).unwrap_or(&0));
    }
    Ok(())
}

/// Strip formatting from ISBN (hyphens, spaces). Keeps digits and X.
fn normalize_isbn(isbn: Option<String>) -> Option<String> {
    isbn.map(|s| {
        let cleaned: String = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
            .collect();
        if cleaned.is_empty() { s } else { cleaned }
    })
}

/// List all books with optional filters
pub async fn list_books(
    db: &DatabaseConnection,
    filter: BookFilter,
) -> Result<Vec<Book>, ServiceError> {
    tracing::info!(
        "List books - Filters: status={:?}, title={:?}, tag={:?}",
        filter.status,
        filter.title,
        filter.tag
    );

    let mut query = BookEntity::find();

    // Apply DB-level filters
    if let Some(status) = &filter.status
        && !status.is_empty()
    {
        query = query.filter(crate::models::book::Column::ReadingStatus.eq(status));
    }

    if let Some(title) = &filter.title
        && !title.is_empty()
    {
        query = query.filter(crate::models::book::Column::Title.contains(title));
    }

    if let Some(tag) = &filter.tag
        && !tag.is_empty()
    {
        query = query.filter(crate::models::book::Column::Subjects.contains(tag));
    }

    // Eager-load authors: 2 queries instead of N+1
    let books_with_authors: Vec<(
        crate::models::book::Model,
        Vec<crate::models::author::Model>,
    )> = query
        .order_by_asc(crate::models::book::Column::ShelfPosition)
        .find_with_related(crate::models::author::Entity)
        .all(db)
        .await?;

    tracing::info!("DB query returned {} books", books_with_authors.len());

    // Batch-fetch lent/borrowed sets for the owner-only reading_status
    // override below. `available_copies` is populated separately via the
    // shared `populate_available_copies` helper so HTTP peer-facing paths
    // stay consistent with this FRB path.
    let book_ids: Vec<i32> = books_with_authors.iter().map(|(m, _)| m.id).collect();
    let mut lent_set: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut borrowed_set: std::collections::HashSet<i32> = std::collections::HashSet::new();
    if !book_ids.is_empty()
        && let Ok(copies) = crate::models::copy::Entity::find()
            .filter(crate::models::copy::Column::BookId.is_in(book_ids))
            .all(db)
            .await
    {
        for c in &copies {
            if c.status == "loaned" {
                lent_set.insert(c.book_id);
            }
            if c.status == "borrowed" && c.is_temporary {
                borrowed_set.insert(c.book_id);
            }
        }
    }

    let mut book_dtos = Vec::new();

    for (book_model, authors) in books_with_authors {
        let mut book_dto = Book::from(book_model);

        if !authors.is_empty() {
            book_dto.author = Some(
                authors
                    .into_iter()
                    .map(|a| a.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }

        // Override reading_status based on copy status:
        // - Temporary copy borrowed → I borrowed this book from someone
        // - Own copy borrowed → I lent this book to someone
        if borrowed_set.contains(&book_dto.id.unwrap_or(0)) {
            book_dto.reading_status = Some("borrowed".to_string());
        } else if lent_set.contains(&book_dto.id.unwrap_or(0)) {
            book_dto.reading_status = Some("lent".to_string());
        }

        // In-memory status filter (safety net)
        if let Some(status_filter) = &filter.status
            && !status_filter.is_empty()
        {
            if let Some(book_status) = &book_dto.reading_status {
                if book_status != status_filter {
                    continue;
                }
            } else {
                continue;
            }
        }

        // In-memory author filter
        if let Some(author_query) = &filter.author
            && !author_query.is_empty()
        {
            if let Some(authors) = &book_dto.author {
                if !authors
                    .to_lowercase()
                    .contains(&author_query.to_lowercase())
                {
                    continue;
                }
            } else {
                continue;
            }
        }

        book_dtos.push(book_dto);
    }

    populate_available_copies(db, &mut book_dtos).await?;

    tracing::info!("Returning {} books after filters", book_dtos.len());
    Ok(book_dtos)
}

/// Get a single book by ID
pub async fn get_book(db: &DatabaseConnection, id: i32) -> Result<Book, ServiceError> {
    let book_model = BookEntity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    let mut book_dto = Book::from(book_model.clone());

    // Fetch authors
    if let Ok(authors) = book_model
        .find_related(crate::models::author::Entity)
        .all(db)
        .await
        && !authors.is_empty()
    {
        book_dto.author = Some(
            authors
                .into_iter()
                .map(|a| a.name)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }

    Ok(book_dto)
}

/// Create a new book
pub async fn create_book(db: &DatabaseConnection, book: Book) -> Result<Book, ServiceError> {
    let now = chrono::Utc::now();

    let reading_status = book
        .reading_status
        .clone()
        .unwrap_or_else(|| "to_read".to_string());
    validate_reading_status(&reading_status)?;

    let subjects_json = book
        .subjects
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

    let new_book = BookActiveModel {
        title: Set(book.title.clone()),
        isbn: Set(normalize_isbn(book.isbn.clone())),
        summary: Set(book.summary.clone()),
        publisher: Set(book.publisher.clone()),
        publication_year: Set(book.publication_year),
        dewey_decimal: Set(book.dewey_decimal.clone()),
        lcc: Set(book.lcc.clone()),
        subjects: Set(subjects_json),
        marc_record: Set(book.marc_record.clone()),
        cataloguing_notes: Set(book.cataloguing_notes.clone()),
        source_data: Set(book.source_data.clone()),
        cover_url: Set(book.cover_url.clone()),
        reading_status: Set(reading_status),
        started_reading_at: Set(book.started_reading_at.clone().flatten()),
        finished_reading_at: Set(book.finished_reading_at.clone().flatten()),
        owned: Set(book.owned.unwrap_or(true)),
        price: Set(book.price),
        created_at: Set(now.to_rfc3339()),
        updated_at: Set(now.to_rfc3339()),
        ..Default::default()
    };

    let mut model = new_book.insert(db).await?;

    // Deferred enrichment: fetch OL description if summary is empty
    if model.summary.is_none()
        && let Some(ref sd) = model.source_data
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(sd)
        && json.get("source").and_then(|s| s.as_str()) == Some("openlibrary")
        && let Some(work_key) = json.get("key").and_then(|k| k.as_str())
        && let Some(desc) = crate::api::integrations::fetch_work_description(work_key).await
    {
        let mut update: BookActiveModel = model.clone().into();
        update.summary = Set(Some(desc));
        if let Ok(updated) = update.update(db).await {
            model = updated;
        }
    }

    // Handle author if provided
    if let Some(author_name) = book.author {
        let _ = create_or_link_author(db, model.id, &author_name).await;
    }

    // Log sync operation (minimal payload, no sensitive data)
    let _ = crate::sync::log_operation(db, "book", model.id, "INSERT", None).await;

    // Create default copy if book is owned (wishlist items with owned=false skip this)
    if model.owned {
        if let Ok(Some(library)) = crate::models::library::Entity::find().one(db).await {
            let copy = crate::models::copy::ActiveModel {
                book_id: Set(model.id),
                library_id: Set(library.id),
                status: Set("available".to_string()),
                is_temporary: Set(false),
                created_at: Set(now.to_rfc3339()),
                updated_at: Set(now.to_rfc3339()),
                ..Default::default()
            };
            if let Ok(saved_copy) = copy.insert(db).await {
                let _ = crate::sync::log_operation(
                    db,
                    "copy",
                    saved_copy.id,
                    "INSERT",
                    Some(serde_json::json!({ "book_id": model.id })),
                )
                .await;
            }
        } else {
            tracing::warn!(
                "Skipping auto-copy creation: no library found for book {}",
                model.id
            );
        }
    }

    Ok(Book::from(model))
}

/// Validates that the reading status is one of the allowed values
fn validate_reading_status(status: &str) -> Result<(), ServiceError> {
    match status {
        "to_read" | "reading" | "read" | "wanting" | "abandoned" => Ok(()),
        _ => Err(ServiceError::InvalidInput(format!(
            "Invalid reading status: '{}'",
            status
        ))),
    }
}

/// Update an existing book
pub async fn update_book(
    db: &DatabaseConnection,
    id: i32,
    book_data: Book,
) -> Result<Book, ServiceError> {
    let now = chrono::Utc::now();

    let book_model = BookEntity::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    let mut book: BookActiveModel = book_model.into();

    book.title = Set(book_data.title);
    book.isbn = Set(normalize_isbn(book_data.isbn));
    book.summary = Set(book_data.summary);
    book.publisher = Set(book_data.publisher);
    book.publication_year = Set(book_data.publication_year);
    if let Some(status) = book_data.reading_status {
        validate_reading_status(&status)?;
        book.reading_status = Set(status);
    }
    if let Some(finished_at) = book_data.finished_reading_at {
        book.finished_reading_at = Set(finished_at);
    }
    if let Some(started_at) = book_data.started_reading_at {
        book.started_reading_at = Set(started_at);
    }
    if let Some(subjects) = book_data.subjects {
        let subjects_json = serde_json::to_string(&subjects).unwrap_or_else(|_| "[]".to_string());
        book.subjects = Set(Some(subjects_json));
    }
    book.user_rating = Set(book_data.user_rating);
    book.cover_url = Set(book_data.cover_url);
    if let Some(owned_value) = book_data.owned {
        book.owned = Set(owned_value);
    }
    if let Some(private_value) = book_data.private {
        book.private = Set(private_value);
    }
    book.price = Set(book_data.price);
    book.page_count = Set(book_data.page_count);
    book.digital_formats = Set(book_data
        .digital_formats
        .map(|f| serde_json::to_string(&f).unwrap_or_else(|_| "[]".to_string())));

    book.updated_at = Set(now.to_rfc3339());

    let model = book.update(db).await?;

    let _ = crate::sync::log_operation(db, "book", id, "UPDATE", None).await;

    // Handle author update: if author field is provided, update the book_authors join table
    let author_names: Vec<String> = if let Some(ref authors_list) = book_data.authors {
        authors_list.clone()
    } else if let Some(ref author_str) = book_data.author {
        author_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };

    if !author_names.is_empty() {
        use crate::models::author::{self, ActiveModel as AuthorActive, Entity as AuthorEntity};
        use crate::models::book_authors::{self, ActiveModel as BookAuthorActive};

        // Remove existing author links
        book_authors::Entity::delete_many()
            .filter(book_authors::Column::BookId.eq(id))
            .exec(db)
            .await?;

        let _ = crate::sync::log_operation(
            db,
            "book_author",
            id,
            "DELETE",
            Some(serde_json::json!({ "book_id": id })),
        )
        .await;

        // Find or create each author and link to book
        for author_name in author_names {
            let author_model = match AuthorEntity::find()
                .filter(author::Column::Name.eq(&author_name))
                .one(db)
                .await?
            {
                Some(existing) => existing,
                None => {
                    let new_author = AuthorActive {
                        name: Set(author_name),
                        created_at: Set(now.to_rfc3339()),
                        updated_at: Set(now.to_rfc3339()),
                        ..Default::default()
                    };
                    let created = new_author.insert(db).await?;
                    let _ =
                        crate::sync::log_operation(db, "author", created.id, "INSERT", None).await;
                    created
                }
            };

            let book_author = BookAuthorActive {
                book_id: Set(id),
                author_id: Set(author_model.id),
                ..Default::default()
            };
            if book_author.insert(db).await.is_ok() {
                let _ = crate::sync::log_operation(
                    db,
                    "book_author",
                    id,
                    "INSERT",
                    Some(serde_json::json!({ "book_id": id, "author_id": author_model.id })),
                )
                .await;
            }
        }
    }

    Ok(Book::from(model))
}

/// Delete a book by ID
pub async fn delete_book(db: &DatabaseConnection, id: i32) -> Result<(), ServiceError> {
    BookEntity::delete_by_id(id).exec(db).await?;

    let _ = crate::sync::log_operation(db, "book", id, "DELETE", None).await;

    // Best-effort: remove the orphaned cover from the hub so storage
    // does not grow indefinitely. A failure here (hub unreachable, not
    // registered, cover never existed) must not fail the deletion
    // itself — the book is already gone from the local DB.
    let hub_svc = crate::services::hub_directory_service::HubDirectoryService::new();
    if let Err(e) = hub_svc.delete_cover(db, id).await {
        tracing::debug!("hub cover cleanup skipped for book {id}: {e}");
    }

    Ok(())
}

/// List all unique tags with counts
pub async fn list_tags(db: &DatabaseConnection) -> Result<Vec<TagDto>, ServiceError> {
    let books = BookEntity::find().all(db).await?;

    let mut tag_counts: HashMap<String, usize> = HashMap::new();

    for book in books {
        if let Some(subjects_json) = book.subjects
            && let Ok(subjects) = serde_json::from_str::<Vec<String>>(&subjects_json)
        {
            for subject in subjects {
                if !subject.trim().is_empty() {
                    *tag_counts.entry(subject.trim().to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    let mut tags: Vec<TagDto> = tag_counts
        .into_iter()
        .map(|(name, count)| TagDto { name, count })
        .collect();

    tags.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));

    Ok(tags)
}

/// Reorder books by updating shelf_position
pub async fn reorder_books(
    db: &DatabaseConnection,
    book_ids: Vec<i32>,
) -> Result<(), ServiceError> {
    let txn = db.begin().await?;

    for (index, book_id) in book_ids.iter().enumerate() {
        BookEntity::update_many()
            .col_expr(
                crate::models::book::Column::ShelfPosition,
                sea_orm::sea_query::Expr::value(index as i32),
            )
            .filter(crate::models::book::Column::Id.eq(*book_id))
            .exec(&txn)
            .await?;
    }

    txn.commit().await?;
    Ok(())
}

/// Count total books
pub async fn count_books(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    use sea_orm::PaginatorTrait;
    let count = BookEntity::find().count(db).await?;
    Ok(count as i64)
}

/// Enrich books that have an ISBN but no cover by checking external sources.
/// Runs sequentially with throttling to avoid hammering APIs.
/// Returns the number of covers found and persisted.
///
/// Sources tried per book: Inventaire → OpenLibrary → Google Books (if enabled).
/// Uses `BookRepository` trait for data access (clean architecture).
/// Profile lookup for module toggles still uses `DatabaseConnection` directly
/// (installation_profile does not have its own repository yet).
pub async fn enrich_missing_covers(
    db: &DatabaseConnection,
    book_repo: &dyn crate::domain::BookRepository,
) -> Result<i32, ServiceError> {
    use crate::models::installation_profile::Entity as ProfileEntity;

    // Cleanup: re-validate existing OpenLibrary cover URLs that may be
    // false positives (stored before the ?default=false validation fix).
    // OpenLibrary returns 200 + redirect for ALL ISBNs unless ?default=false
    // is used, so previously stored URLs may point to 1x1 transparent pixels.
    cleanup_stale_openlibrary_covers(db).await;

    let books = book_repo
        .find_missing_covers()
        .await
        .map_err(|e| ServiceError::Database(format!("{e}")))?;
    if books.is_empty() {
        return Ok(0);
    }

    let (enable_inventaire, enable_google) = match ProfileEntity::find_by_id(1).one(db).await {
        Ok(Some(profile)) => {
            let modules: Vec<String> =
                serde_json::from_str(&profile.enabled_modules).unwrap_or_default();
            (
                !modules.contains(&"disable_fallback:inventaire".to_string()),
                modules.contains(&"enable_google_books".to_string()),
            )
        }
        _ => (true, false),
    };

    let gb_api_key = load_google_books_api_key(db).await;

    let total = books.len();
    let mut enriched = 0i32;

    for (book_id, isbn) in &books {
        if let Some(url) = find_cover_url(
            isbn,
            enable_inventaire,
            enable_google,
            gb_api_key.as_deref(),
        )
        .await
        {
            book_repo
                .update_cover_url(*book_id, &url)
                .await
                .map_err(|e| ServiceError::Database(format!("{e}")))?;
            enriched += 1;
        }

        // Throttle: 500ms between books to avoid hammering APIs
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    tracing::info!("Cover enrichment: {enriched}/{total} covers found and persisted");
    Ok(enriched)
}

/// Search for a cover URL for a single ISBN from all available external sources.
/// Returns the URL if found, None otherwise.
pub async fn search_cover_for_book(
    db: &DatabaseConnection,
    isbn: &str,
) -> Result<Option<String>, ServiceError> {
    use crate::models::installation_profile::Entity as ProfileEntity;

    let (enable_inventaire, enable_google, enable_bnf) =
        match ProfileEntity::find_by_id(1).one(db).await {
            Ok(Some(profile)) => {
                let modules: Vec<String> =
                    serde_json::from_str(&profile.enabled_modules).unwrap_or_default();
                (
                    !modules.contains(&"disable_fallback:inventaire".to_string()),
                    modules.contains(&"enable_google_books".to_string()),
                    !modules.contains(&"disable_fallback:bnf".to_string()),
                )
            }
            _ => (true, false, true),
        };

    // Try Inventaire first (best cover coverage, especially for non-English books)
    if enable_inventaire
        && let Ok(metadata) =
            crate::modules::integrations::inventaire::fetch_inventaire_metadata(isbn).await
        && let Some(url) = metadata.cover_url
    {
        return Ok(Some(url));
    }

    // Try OpenLibrary (lightweight HEAD check)
    if let Some(url) = crate::modules::integrations::openlibrary::fetch_cover_url(isbn).await {
        return Ok(Some(url));
    }

    // For French ISBNs, try BNF
    let clean_isbn = isbn.replace('-', "");
    let is_french = clean_isbn.starts_with("9782") || clean_isbn.starts_with("97910");
    if enable_bnf
        && is_french
        && let Ok(Some(bnf_book)) = crate::modules::integrations::bnf::lookup_bnf_isbn(isbn).await
        && let Some(url) = bnf_book.cover_url
    {
        return Ok(Some(url));
    }

    // Fallback to Google Books
    if enable_google {
        let gb_api_key = load_google_books_api_key(db).await;
        if let Some(url) =
            crate::modules::integrations::google_books::fetch_cover_url(isbn, gb_api_key.as_deref())
                .await
        {
            return Ok(Some(url));
        }
    }

    Ok(None)
}

/// Search for a cover URL by title, with author verification when possible.
/// Used as a fallback when ISBN-based search finds nothing.
/// Tries Inventaire first, then Google Books.
/// When an author is provided, verifies it matches to avoid wrong covers.
/// When no author is available, returns the first result with a cover
/// (the user confirms via a dialog before applying).
pub async fn search_cover_by_title(
    title: &str,
    author: Option<&str>,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Result<Option<String>, ServiceError> {
    let author_lower = author.filter(|a| !a.is_empty()).map(|a| a.to_lowercase());

    // 1. Try Inventaire
    tracing::info!("search_cover_by_title: trying Inventaire for '{}'", title);
    if let Ok(results) = crate::modules::integrations::inventaire::search_inventaire(title).await {
        tracing::info!(
            "search_cover_by_title: Inventaire returned {} results",
            results.len()
        );
        for result in &results {
            let Some(ref image_url) = result.image else {
                continue;
            };

            if let Some(ref al) = author_lower {
                // Verify author: check authors field, then description
                let authors_match = result.authors.as_ref().is_some_and(|authors| {
                    authors.iter().any(|ra| {
                        let ra_lower = ra.to_lowercase();
                        ra_lower.contains(al) || al.contains(&ra_lower)
                    })
                });
                let desc_match = !authors_match
                    && result
                        .description
                        .as_ref()
                        .is_some_and(|d| d.to_lowercase().contains(al));

                if !authors_match && !desc_match {
                    continue;
                }
            }
            // No author to verify, or author matched
            return Ok(Some(image_url.clone()));
        }
    }

    // 2. Try Google Books by title (if enabled by user)
    if enable_google {
        let query = crate::api::search::SearchQuery {
            q: Some(title.to_string()),
            title: None,
            author: None,
            publisher: None,
            year_min: None,
            year_max: None,
            tags: None,
            subjects: None,
            sources: None,
            autocomplete: Some(true),
        };
        let books =
            crate::modules::integrations::google_books::search_books(&query, google_api_key).await;
        tracing::info!(
            "search_cover_by_title: Google Books returned {} results",
            books.len()
        );
        for book in &books {
            let Some(ref cover_url) = book.cover_url else {
                continue;
            };

            if let Some(ref al) = author_lower {
                // Verify author from source_data
                let source_authors: Vec<String> = book
                    .source_data
                    .as_ref()
                    .and_then(|sd| serde_json::from_str::<serde_json::Value>(sd).ok())
                    .and_then(|v| {
                        v.get("authors")
                            .and_then(|a| serde_json::from_value::<Vec<String>>(a.clone()).ok())
                    })
                    .unwrap_or_default();

                let match_found = source_authors.iter().any(|ra| {
                    let ra_lower = ra.to_lowercase();
                    ra_lower.contains(al) || al.contains(&ra_lower)
                });
                if !match_found {
                    continue;
                }
            }
            return Ok(Some(cover_url.clone()));
        }
    }

    Ok(None)
}

/// Search ALL enabled cover sources in parallel for a given ISBN.
/// Returns all found cover candidates (may be empty).
/// Unlike `search_cover_for_book`, this does NOT stop at the first hit.
pub async fn search_all_covers_for_book(
    db: &DatabaseConnection,
    isbn: &str,
) -> Result<Vec<CoverCandidate>, ServiceError> {
    use crate::models::installation_profile::Entity as ProfileEntity;

    let (enable_inventaire, enable_google, enable_bnf) =
        match ProfileEntity::find_by_id(1).one(db).await {
            Ok(Some(profile)) => {
                let modules: Vec<String> =
                    serde_json::from_str(&profile.enabled_modules).unwrap_or_default();
                (
                    !modules.contains(&"disable_fallback:inventaire".to_string()),
                    modules.contains(&"enable_google_books".to_string()),
                    !modules.contains(&"disable_fallback:bnf".to_string()),
                )
            }
            _ => (true, false, true),
        };

    let clean_isbn = isbn.replace('-', "");
    let is_french = clean_isbn.starts_with("9782") || clean_isbn.starts_with("97910");

    let inventaire_fut = async {
        if !enable_inventaire {
            return None;
        }
        crate::modules::integrations::inventaire::fetch_inventaire_metadata(isbn)
            .await
            .ok()
            .and_then(|m| m.cover_url)
            .map(|url| CoverCandidate {
                url,
                source: "Inventaire".to_string(),
                language: None,
            })
    };

    let openlibrary_fut = async {
        crate::modules::integrations::openlibrary::fetch_cover_url(isbn)
            .await
            .map(|url| CoverCandidate {
                url,
                source: "OpenLibrary".to_string(),
                language: None,
            })
    };

    let bnf_fut = async {
        if !enable_bnf || !is_french {
            return None;
        }
        crate::modules::integrations::bnf::lookup_bnf_isbn(isbn)
            .await
            .ok()
            .flatten()
            .and_then(|b| b.cover_url)
            .map(|url| CoverCandidate {
                url,
                source: "BNF".to_string(),
                language: None,
            })
    };

    let gb_api_key = if enable_google {
        load_google_books_api_key(db).await
    } else {
        None
    };

    let google_fut = async {
        if !enable_google {
            return None;
        }
        crate::modules::integrations::google_books::fetch_cover_url(isbn, gb_api_key.as_deref())
            .await
            .map(|url| CoverCandidate {
                url,
                source: "Google Books".to_string(),
                language: None,
            })
    };

    let (inv, ol, bnf, gb) = tokio::join!(inventaire_fut, openlibrary_fut, bnf_fut, google_fut);

    let mut candidates = Vec::new();
    if let Some(c) = inv {
        candidates.push(c);
    }
    if let Some(c) = ol {
        candidates.push(c);
    }
    if let Some(c) = bnf {
        candidates.push(c);
    }
    if let Some(c) = gb {
        candidates.push(c);
    }

    {
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.url.clone()));
    }
    Ok(candidates)
}

/// Search ALL enabled sources by title in parallel, collecting all cover candidates.
/// Used as fallback when ISBN-based search returns too few results.
pub async fn search_all_covers_by_title(
    db: &DatabaseConnection,
    title: &str,
    author: Option<&str>,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Result<Vec<CoverCandidate>, ServiceError> {
    let author_lower = author.filter(|a| !a.is_empty()).map(|a| a.to_lowercase());

    // Get the book's language from DB (stored in source_data JSON) for sorting covers.
    // Try "languages" array (OpenLibrary/BNF) then "language" string (Google Books).
    // Fallback to "fr" (consistent with Inventaire search default).
    let book_lang: String = BookEntity::find()
        .filter(crate::models::book::Column::Title.eq(title))
        .one(db)
        .await
        .ok()
        .flatten()
        .and_then(|b| {
            b.source_data.as_ref().and_then(|sd| {
                serde_json::from_str::<serde_json::Value>(sd)
                    .ok()
                    .and_then(|json| {
                        json.get("languages")
                            .and_then(|l| l.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                json.get("language")
                                    .and_then(|l| l.as_str())
                                    .map(|s| s.to_string())
                            })
                    })
            })
        })
        .unwrap_or_else(|| "fr".to_string());

    let inv_fut = {
        let title = title.to_string();
        let author_lower = author_lower.clone();
        let author_for_query = author.filter(|a| !a.is_empty()).map(|s| s.to_string());
        async move {
            let mut results = Vec::new();
            let search_query = match &author_for_query {
                Some(a) => format!("{} {}", title, a),
                None => title.clone(),
            };
            if let Ok(items) =
                crate::modules::integrations::inventaire::search_inventaire(&search_query).await
            {
                for item in &items {
                    // Author filter: skip items that don't match the expected author
                    if let Some(ref al) = author_lower {
                        let authors_match = item.authors.as_ref().is_some_and(|authors| {
                            authors.iter().any(|ra| author_tokens_match(al, ra))
                        });
                        let desc_match = !authors_match
                            && item
                                .description
                                .as_ref()
                                .is_some_and(|d| d.to_lowercase().contains(al));
                        if !authors_match && !desc_match {
                            continue;
                        }
                    }

                    // Fetch edition covers (with language) for this matching work
                    let edition_covers =
                        crate::modules::integrations::inventaire::fetch_work_edition_covers(
                            &item.uri,
                        )
                        .await;
                    if edition_covers.is_empty() {
                        // Fallback: work-level image when no editions found
                        if let Some(ref image_url) = item.image {
                            results.push(CoverCandidate {
                                url: image_url.clone(),
                                source: "Inventaire".to_string(),
                                language: None,
                            });
                        }
                    } else {
                        for ec in edition_covers {
                            results.push(CoverCandidate {
                                url: ec.url,
                                source: "Inventaire".to_string(),
                                language: ec.lang,
                            });
                        }
                    }
                }
            }
            results
        }
    };

    let gb_fut = {
        let title = title.to_string();
        let author_orig = author.filter(|a| !a.is_empty()).map(|s| s.to_string());
        let author_lower = author_lower.clone();
        let gb_key = google_api_key.map(|s| s.to_string());
        async move {
            if !enable_google {
                return Vec::new();
            }
            let query = crate::api::search::SearchQuery {
                q: None,
                title: Some(title),
                author: author_orig,
                publisher: None,
                year_min: None,
                year_max: None,
                tags: None,
                subjects: None,
                sources: None,
                autocomplete: Some(true),
            };
            let books =
                crate::modules::integrations::google_books::search_books(&query, gb_key.as_deref())
                    .await;
            let mut results = Vec::new();
            for book in &books {
                let Some(ref cover_url) = book.cover_url else {
                    continue;
                };
                if let Some(ref al) = author_lower {
                    let source_authors: Vec<String> = book
                        .source_data
                        .as_ref()
                        .and_then(|sd| serde_json::from_str::<serde_json::Value>(sd).ok())
                        .and_then(|v| {
                            v.get("authors")
                                .and_then(|a| serde_json::from_value::<Vec<String>>(a.clone()).ok())
                        })
                        .unwrap_or_default();
                    let match_found = source_authors.iter().any(|ra| author_tokens_match(al, ra));
                    if !match_found {
                        continue;
                    }
                }
                // Extract language from Google Books source_data
                let gb_lang = book
                    .source_data
                    .as_ref()
                    .and_then(|sd| serde_json::from_str::<serde_json::Value>(sd).ok())
                    .and_then(|v| {
                        v.get("language")
                            .and_then(|l| l.as_str().map(|s| s.to_string()))
                    });
                results.push(CoverCandidate {
                    url: cover_url.clone(),
                    source: "Google Books".to_string(),
                    language: gb_lang,
                });
            }
            results
        }
    };

    let (inv_results, gb_results) = tokio::join!(inv_fut, gb_fut);

    let mut candidates = Vec::new();
    candidates.extend(inv_results);
    candidates.extend(gb_results);
    {
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.url.clone()));
    }

    // Sort by language match: matching lang first, unknown middle, non-matching last
    let lang_base = book_lang
        .split('-')
        .next()
        .unwrap_or(&book_lang)
        .to_lowercase();
    candidates.sort_by_key(|c| match &c.language {
        Some(cl) => {
            let cl_base = cl.split('-').next().unwrap_or(cl).to_lowercase();
            if cl_base == lang_base { 0 } else { 2 }
        }
        None => 1,
    });

    Ok(candidates)
}

/// Try to find a cover URL for an ISBN from multiple sources.
/// Used by batch enrichment.
async fn find_cover_url(
    isbn: &str,
    enable_inventaire: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<String> {
    // Inventaire (best coverage for non-English books)
    if enable_inventaire
        && let Ok(metadata) =
            crate::modules::integrations::inventaire::fetch_inventaire_metadata(isbn).await
        && metadata.cover_url.is_some()
    {
        return metadata.cover_url;
    }

    // OpenLibrary (lightweight HEAD check)
    if let Some(url) = crate::modules::integrations::openlibrary::fetch_cover_url(isbn).await {
        return Some(url);
    }

    // Google Books
    if enable_google
        && let Some(url) =
            crate::modules::integrations::google_books::fetch_cover_url(isbn, google_api_key).await
    {
        return Some(url);
    }

    None
}

/// One-time cleanup: re-validate OpenLibrary cover URLs stored before the
/// `?default=false` fix. Without that parameter, OpenLibrary returns 200 for ALL
/// ISBNs (redirecting to a 1x1 transparent pixel), so many stored URLs are
/// false positives. This clears invalid ones so they can be re-enriched from
/// better sources (Inventaire, BNF, etc.).
async fn cleanup_stale_openlibrary_covers(db: &DatabaseConnection) {
    use crate::models::book::Column;

    let stale_models = match BookEntity::find()
        .filter(Column::CoverUrl.starts_with("https://covers.openlibrary.org/b/isbn/"))
        .filter(Column::Isbn.is_not_null())
        .all(db)
        .await
    {
        Ok(models) => models,
        Err(_) => return,
    };

    if stale_models.is_empty() {
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut cleared = 0;
    for model in &stale_models {
        let isbn = model.isbn.as_deref().unwrap_or("").trim();
        if isbn.is_empty() {
            continue;
        }

        let check_url = format!(
            "https://covers.openlibrary.org/b/isbn/{}-L.jpg?default=false",
            isbn
        );

        let is_valid = match client.head(&check_url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => true, // Keep on network error (don't delete data on failure)
        };

        if !is_valid {
            let _ = BookEntity::update_many()
                .col_expr(
                    Column::CoverUrl,
                    sea_orm::sea_query::Expr::value(Option::<String>::None),
                )
                .filter(Column::Id.eq(model.id))
                .exec(db)
                .await;
            cleared += 1;
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    if cleared > 0 {
        tracing::info!(
            "Cleared {cleared}/{} stale OpenLibrary cover URLs",
            stale_models.len()
        );
    }
}

/// Load the Google Books API key from the installation profile.
async fn load_google_books_api_key(db: &DatabaseConnection) -> Option<String> {
    use crate::models::installation_profile::Entity as ProfileEntity;

    if let Ok(Some(profile)) = ProfileEntity::find_by_id(1).one(db).await {
        let api_keys: std::collections::HashMap<String, String> = profile
            .api_keys
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        return api_keys.get("google_books").cloned();
    }
    None
}

// Helper: Create or link author to book
async fn create_or_link_author(
    db: &DatabaseConnection,
    book_id: i32,
    author_name: &str,
) -> Result<(), ServiceError> {
    use crate::models::author::{ActiveModel as AuthorActive, Entity as AuthorEntity};
    use crate::models::book_authors::ActiveModel as BookAuthorActive;

    let now = chrono::Utc::now();

    // Find or create author
    let author = match AuthorEntity::find()
        .filter(crate::models::author::Column::Name.eq(author_name))
        .one(db)
        .await?
    {
        Some(existing) => existing,
        None => {
            let new_author = AuthorActive {
                name: Set(author_name.to_string()),
                created_at: Set(now.to_rfc3339()),
                updated_at: Set(now.to_rfc3339()),
                ..Default::default()
            };
            let created = new_author.insert(db).await?;
            let _ = crate::sync::log_operation(db, "author", created.id, "INSERT", None).await;
            created
        }
    };

    // Create book-author relation
    let book_author = BookAuthorActive {
        book_id: Set(book_id),
        author_id: Set(author.id),
        ..Default::default()
    };
    if book_author.insert(db).await.is_ok() {
        let _ = crate::sync::log_operation(
            db,
            "book_author",
            book_id,
            "INSERT",
            Some(serde_json::json!({ "book_id": book_id, "author_id": author.id })),
        )
        .await;
    }

    Ok(())
}

/// Token-based author name matching for cover search filtering.
/// Splits the query author on whitespace/commas, then checks that every token
/// appears somewhere in the candidate string (case-insensitive).
/// Handles name order differences: "Jean-Paul Sartre" matches "Sartre, Jean-Paul".
fn author_tokens_match(query_author: &str, candidate_author: &str) -> bool {
    let q = query_author.to_lowercase();
    let c = candidate_author.to_lowercase();
    if q.is_empty() || c.is_empty() {
        return false;
    }
    // Fast path: one contains the other entirely
    if q.contains(&c) || c.contains(&q) {
        return true;
    }
    // Token-based: split on whitespace and commas (hyphens preserved for compound names)
    let q_tokens: Vec<&str> = q
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .filter(|s| !s.is_empty())
        .collect();
    if q_tokens.is_empty() {
        return false;
    }
    q_tokens.iter().all(|t| c.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_author_tokens_match_same_order() {
        assert!(author_tokens_match("Jean-Paul Sartre", "Jean-Paul Sartre"));
    }

    #[test]
    fn test_author_tokens_match_reversed_order() {
        assert!(author_tokens_match("Jean-Paul Sartre", "Sartre, Jean-Paul"));
    }

    #[test]
    fn test_author_tokens_match_case_insensitive() {
        assert!(author_tokens_match("victor hugo", "Victor Hugo"));
    }

    #[test]
    fn test_author_tokens_match_substring() {
        assert!(author_tokens_match("Hugo", "Victor Hugo"));
    }

    #[test]
    fn test_author_tokens_match_no_match() {
        assert!(!author_tokens_match("Albert Camus", "Jean-Paul Sartre"));
    }

    #[test]
    fn test_author_tokens_match_partial_token_no_false_positive() {
        assert!(!author_tokens_match("Paul Martin", "Jean-Paul Sartre"));
    }

    #[test]
    fn test_author_tokens_match_empty_query() {
        assert!(!author_tokens_match("", "Victor Hugo"));
    }

    #[test]
    fn test_author_tokens_match_accented_names() {
        assert!(author_tokens_match("Emile Zola", "Zola, Emile"));
    }

    async fn insert_test_book(db: &DatabaseConnection, title: &str) -> i32 {
        use crate::models::book;
        use sea_orm::Set;
        let now = chrono::Utc::now().to_rfc3339();
        book::Entity::insert(book::ActiveModel {
            title: Set(title.to_owned()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap()
        .last_insert_id
    }

    async fn insert_test_copy(
        db: &DatabaseConnection,
        book_id: i32,
        status: &str,
        is_temporary: bool,
    ) {
        use crate::models::copy;
        use sea_orm::Set;
        let now = chrono::Utc::now().to_rfc3339();
        copy::Entity::insert(copy::ActiveModel {
            book_id: Set(book_id),
            library_id: Set(0),
            status: Set(status.to_owned()),
            is_temporary: Set(is_temporary),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
    }

    /// `populate_available_copies` must count only copies whose status is
    /// exactly "available" (not "loaned", "borrowed", etc.). This is the
    /// field the peer-lib carousel filter relies on to hide fully-lent
    /// books cached on the iPhone.
    #[tokio::test]
    async fn populate_available_copies_counts_only_available_status() {
        use crate::db;
        use sea_orm::{ConnectionTrait, Statement};

        let db = db::init_db("sqlite::memory:").await.unwrap();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();

        let id_two_avail = insert_test_book(&db, "Two available").await;
        let id_all_lent = insert_test_book(&db, "All lent out").await;
        let id_peer_borrowed = insert_test_book(&db, "I borrowed it").await;
        let id_no_copies = insert_test_book(&db, "Standalone metadata").await;

        insert_test_copy(&db, id_two_avail, "available", false).await;
        insert_test_copy(&db, id_two_avail, "available", false).await;
        insert_test_copy(&db, id_two_avail, "loaned", false).await;
        insert_test_copy(&db, id_all_lent, "loaned", false).await;
        insert_test_copy(&db, id_peer_borrowed, "borrowed", true).await;

        let mut books = vec![
            Book {
                id: Some(id_two_avail),
                title: "Two available".to_owned(),
                ..Default::default()
            },
            Book {
                id: Some(id_all_lent),
                title: "All lent out".to_owned(),
                ..Default::default()
            },
            Book {
                id: Some(id_peer_borrowed),
                title: "I borrowed it".to_owned(),
                ..Default::default()
            },
            Book {
                id: Some(id_no_copies),
                title: "Standalone metadata".to_owned(),
                ..Default::default()
            },
        ];
        populate_available_copies(&db, &mut books).await.unwrap();

        assert_eq!(books[0].available_copies, Some(2));
        assert_eq!(
            books[1].available_copies,
            Some(0),
            "all-loaned book must report zero availability so peers drop it",
        );
        assert_eq!(
            books[2].available_copies,
            Some(0),
            "a book with only a temporary borrowed copy must report zero",
        );
        assert_eq!(
            books[3].available_copies,
            Some(0),
            "book with no copies must still be set to Some(0), not None",
        );
    }
}
