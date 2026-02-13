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
        isbn: Set(book.isbn.clone()),
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

    let model = new_book.insert(db).await?;

    // Handle author if provided
    if let Some(author_name) = book.author {
        let _ = create_or_link_author(db, model.id, &author_name).await;
    }

    // Log sync operation
    let _ = crate::sync::log_operation(
        db,
        "book",
        model.id,
        "INSERT",
        Some(serde_json::to_value(&model).unwrap()),
    )
    .await;

    // Create default copy only for individual/kid profiles AND only if book is owned
    // Librarians manage copies manually through inventory
    // Wishlist items (owned=false) should not have copies
    if let Ok(Some(profile)) = crate::models::installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
    {
        let is_individual_profile =
            profile.profile_type == "individual" || profile.profile_type == "kid";
        if is_individual_profile && model.owned {
            // Dynamically get the library_id
            let library = crate::models::library::Entity::find()
                .one(db)
                .await?
                .ok_or(ServiceError::NotFound)?; // Return NotFound if no library exists

            let copy = crate::models::copy::ActiveModel {
                book_id: Set(model.id),
                library_id: Set(library.id), // Use the dynamically fetched library ID
                status: Set("available".to_string()),
                is_temporary: Set(false),
                created_at: Set(now.to_rfc3339()),
                updated_at: Set(now.to_rfc3339()),
                ..Default::default()
            };
            let _ = copy.insert(db).await;
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
    book.isbn = Set(book_data.isbn);
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
    book.price = Set(book_data.price);

    book.updated_at = Set(now.to_rfc3339());

    let model = book.update(db).await?;

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
                    new_author.insert(db).await?
                }
            };

            let book_author = BookAuthorActive {
                book_id: Set(id),
                author_id: Set(author_model.id),
                ..Default::default()
            };
            let _ = book_author.insert(db).await;
        }
    }

    Ok(Book::from(model))
}

/// Delete a book by ID
pub async fn delete_book(db: &DatabaseConnection, id: i32) -> Result<(), ServiceError> {
    BookEntity::delete_by_id(id).exec(db).await?;
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

    let total = books.len();
    let mut enriched = 0i32;

    for (book_id, isbn) in &books {
        if let Some(url) = find_cover_url(isbn, enable_inventaire, enable_google).await {
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
    if enable_google
        && let Some(url) = crate::modules::integrations::google_books::fetch_cover_url(isbn).await
    {
        return Ok(Some(url));
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
        let books = crate::modules::integrations::google_books::search_books(&query).await;
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
            })
    };

    let openlibrary_fut = async {
        crate::modules::integrations::openlibrary::fetch_cover_url(isbn)
            .await
            .map(|url| CoverCandidate {
                url,
                source: "OpenLibrary".to_string(),
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
            })
    };

    let google_fut = async {
        if !enable_google {
            return None;
        }
        crate::modules::integrations::google_books::fetch_cover_url(isbn)
            .await
            .map(|url| CoverCandidate {
                url,
                source: "Google Books".to_string(),
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

    candidates.dedup_by(|a, b| a.url == b.url);
    Ok(candidates)
}

/// Search ALL enabled sources by title in parallel, collecting all cover candidates.
/// Used as fallback when ISBN-based search returns too few results.
pub async fn search_all_covers_by_title(
    title: &str,
    author: Option<&str>,
    enable_google: bool,
) -> Result<Vec<CoverCandidate>, ServiceError> {
    let author_lower = author.filter(|a| !a.is_empty()).map(|a| a.to_lowercase());

    let inv_fut = {
        let title = title.to_string();
        let author_lower = author_lower.clone();
        async move {
            let mut results = Vec::new();
            if let Ok(items) =
                crate::modules::integrations::inventaire::search_inventaire(&title).await
            {
                for item in &items {
                    let Some(ref image_url) = item.image else {
                        continue;
                    };
                    if let Some(ref al) = author_lower {
                        let authors_match = item.authors.as_ref().is_some_and(|authors| {
                            authors.iter().any(|ra| {
                                let ra_lower = ra.to_lowercase();
                                ra_lower.contains(al) || al.contains(&ra_lower)
                            })
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
                    results.push(CoverCandidate {
                        url: image_url.clone(),
                        source: "Inventaire".to_string(),
                    });
                }
            }
            results
        }
    };

    let gb_fut = {
        let title = title.to_string();
        let author_lower = author_lower.clone();
        async move {
            if !enable_google {
                return Vec::new();
            }
            let query = crate::api::search::SearchQuery {
                q: Some(title),
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
            let books = crate::modules::integrations::google_books::search_books(&query).await;
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
                    let match_found = source_authors.iter().any(|ra| {
                        let ra_lower = ra.to_lowercase();
                        ra_lower.contains(al) || al.contains(&ra_lower)
                    });
                    if !match_found {
                        continue;
                    }
                }
                results.push(CoverCandidate {
                    url: cover_url.clone(),
                    source: "Google Books".to_string(),
                });
            }
            results
        }
    };

    let (inv_results, gb_results) = tokio::join!(inv_fut, gb_fut);

    let mut candidates = Vec::new();
    candidates.extend(inv_results);
    candidates.extend(gb_results);
    candidates.dedup_by(|a, b| a.url == b.url);
    Ok(candidates)
}

/// Try to find a cover URL for an ISBN from multiple sources.
/// Used by batch enrichment.
async fn find_cover_url(
    isbn: &str,
    enable_inventaire: bool,
    enable_google: bool,
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
        && let Some(url) = crate::modules::integrations::google_books::fetch_cover_url(isbn).await
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
            new_author.insert(db).await?
        }
    };

    // Create book-author relation
    let book_author = BookAuthorActive {
        book_id: Set(book_id),
        author_id: Set(author.id),
        ..Default::default()
    };
    let _ = book_author.insert(db).await;

    Ok(())
}
