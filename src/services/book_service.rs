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

/// Error type for service operations
#[derive(Debug)]
pub enum ServiceError {
    Database(String),
    NotFound,
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

    let books = query
        .order_by_asc(crate::models::book::Column::ShelfPosition)
        .all(db)
        .await?;

    tracing::info!("DB query returned {} books", books.len());

    let mut book_dtos = Vec::new();

    for book_model in books {
        let mut book_dto = Book::from(book_model.clone());

        // Fetch authors via relation
        if let Ok(authors) = book_model
            .find_related(crate::models::author::Entity)
            .all(db)
            .await
        {
            if !authors.is_empty() {
                book_dto.author = Some(
                    authors
                        .into_iter()
                        .map(|a| a.name)
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
        }

        // Derive cover URLs from ISBN only if no cover is stored
        if book_dto.cover_url.is_none() {
            if let Some(isbn) = &book_dto.isbn {
                book_dto.cover_url = Some(format!(
                    "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                    isbn
                ));
            }
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
    {
        if !authors.is_empty() {
            book_dto.author = Some(
                authors
                    .into_iter()
                    .map(|a| a.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }

    // Derive cover URLs only if no cover is stored
    if book_dto.cover_url.is_none() {
        if let Some(isbn) = &book_dto.isbn {
            book_dto.cover_url = Some(format!(
                "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                isbn
            ));
        }
    }
    if book_dto.large_cover_url.is_none() {
        if let Some(isbn) = &book_dto.isbn {
            book_dto.large_cover_url = Some(format!(
                "https://covers.openlibrary.org/b/isbn/{}-L.jpg",
                isbn
            ));
        }
    }

    Ok(book_dto)
}

/// Create a new book
pub async fn create_book(db: &DatabaseConnection, book: Book) -> Result<Book, ServiceError> {
    let now = chrono::Utc::now();

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
        reading_status: Set(book
            .reading_status
            .clone()
            .unwrap_or_else(|| "to_read".to_string())),
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
            let copy = crate::models::copy::ActiveModel {
                book_id: Set(model.id),
                library_id: Set(1),
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
        if let Some(subjects_json) = book.subjects {
            if let Ok(subjects) = serde_json::from_str::<Vec<String>>(&subjects_json) {
                for subject in subjects {
                    if !subject.trim().is_empty() {
                        *tag_counts.entry(subject.trim().to_string()).or_insert(0) += 1;
                    }
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
