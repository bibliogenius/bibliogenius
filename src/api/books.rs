#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{Value, json};

use crate::models::Book;
use crate::models::book::Entity as BookEntity;

#[derive(serde::Deserialize, Default)]
pub struct BookFilter {
    pub status: Option<String>,
    pub author: Option<String>,
    pub title: Option<String>,
    pub tag: Option<String>,
    pub q: Option<String>,
    pub sort: Option<String>,
    pub page: Option<u64>,
    pub limit: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/api/books",
    params(
        ("status" = Option<String>, Query, description = "Reading status"),
        ("author" = Option<String>, Query, description = "Filter by author name"),
        ("title" = Option<String>, Query, description = "Filter by title"),
        ("tag" = Option<String>, Query, description = "Filter by subject/tag"),
        ("q" = Option<String>, Query, description = "Unified search (Title, ISBN, Subjects)"),
        ("sort" = Option<String>, Query, description = "Sort by: author_asc, title_asc"),
        ("page" = Option<u64>, Query, description = "Page number (0-indexed)"),
        ("limit" = Option<u64>, Query, description = "Items per page")
    ),
    responses(
        (status = 200, description = "List all books")
    )
)]
pub async fn list_books(
    State(state): State<crate::infrastructure::AppState>,
    axum::extract::Query(filter): axum::extract::Query<BookFilter>,
) -> Result<Json<Value>, StatusCode> {
    tracing::info!(
        "List books request - Filters: status={:?}, title={:?}, tag={:?}",
        filter.status,
        filter.title,
        filter.tag
    );

    // Convert API filter to domain filter
    let domain_filter = crate::domain::BookFilter {
        status: filter.status.clone(),
        title: filter.title.clone(),
        author: filter.author.clone(),
        tag: filter.tag.clone(),
        query: filter.q.clone(),
        sort: filter.sort.clone(),
        page: filter.page,
        limit: filter.limit,
    };

    // Fetch via repository
    let result = state.book_repo.find_all(domain_filter).await.map_err(|e| {
        tracing::error!("Failed to fetch books: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!(
        "Repository returned {} books (Total: {})",
        result.books.len(),
        result.total
    );

    let mut book_dtos = result.books;

    // Apply in-memory author sorting only if no pagination (full dataset)
    // Author sorting at DB level requires complex joins not yet implemented
    if filter.limit.is_none()
        && let Some(sort_order) = &filter.sort
        && sort_order == "author_asc"
    {
        book_dtos.sort_by(|a, b| {
            let author_a = a.author.as_deref().unwrap_or("").to_lowercase();
            let author_b = b.author.as_deref().unwrap_or("").to_lowercase();
            if author_a == author_b {
                a.title.to_lowercase().cmp(&b.title.to_lowercase())
            } else {
                author_a.cmp(&author_b)
            }
        });
    }

    Ok(Json(json!({
        "books": book_dtos,
        "total": result.total
    })))
}

#[utoipa::path(
    post,
    path = "/api/books",
    responses(
        (status = 201, description = "Book created successfully"),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn create_book(
    State(state): State<crate::infrastructure::AppState>,
    _claims: crate::auth::Claims,
    Json(book): Json<Book>,
) -> impl IntoResponse {
    let db = state.db();
    let now = chrono::Utc::now();

    // Extract author info before moving book to repository
    let author_names: Vec<String> = if let Some(ref authors) = book.authors {
        authors.clone()
    } else if let Some(ref author) = book.author {
        author
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    };

    // Create book via repository
    match state.book_repo.create(book).await {
        Ok(created_book) => {
            let book_id = created_book.id.expect("Created book must have ID");
            let owned = created_book.owned.unwrap_or(true);

            // Handle authors - find or create, then link to book
            if !author_names.is_empty() {
                use crate::models::author::{ActiveModel as AuthorActive, Entity as AuthorEntity};
                use crate::models::book_authors::ActiveModel as BookAuthorActive;
                use sea_orm::{ColumnTrait, QueryFilter};

                for author_name in author_names {
                    let author = match AuthorEntity::find()
                        .filter(crate::models::author::Column::Name.eq(&author_name))
                        .one(db)
                        .await
                    {
                        Ok(Some(existing)) => existing,
                        _ => {
                            let new_author = AuthorActive {
                                name: Set(author_name),
                                created_at: Set(now.to_rfc3339()),
                                updated_at: Set(now.to_rfc3339()),
                                ..Default::default()
                            };
                            match new_author.insert(db).await {
                                Ok(created) => created,
                                Err(e) => {
                                    tracing::warn!("Failed to create author: {}", e);
                                    continue;
                                }
                            }
                        }
                    };

                    let book_author = BookAuthorActive {
                        book_id: Set(book_id),
                        author_id: Set(author.id),
                        ..Default::default()
                    };
                    let _ = book_author.insert(db).await;
                }
            }

            // Log sync operation
            let _ = crate::sync::log_operation(
                db,
                "book",
                book_id,
                "INSERT",
                Some(serde_json::to_value(&created_book).unwrap_or_default()),
            )
            .await;

            // Create default copy only if owned
            if owned {
                let copy = crate::models::copy::ActiveModel {
                    book_id: Set(book_id),
                    library_id: Set(1),
                    status: Set("available".to_string()),
                    is_temporary: Set(false),
                    created_at: Set(now.to_rfc3339()),
                    updated_at: Set(now.to_rfc3339()),
                    ..Default::default()
                };
                let _ = copy.insert(db).await;
            }

            (
                StatusCode::CREATED,
                Json(json!({
                    "message": "Book created successfully",
                    "book": created_book
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/api/books/{id}",
    params(
        ("id" = i32, Path, description = "Book ID")
    ),
    responses(
        (status = 200, description = "Book deleted successfully"),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn delete_book(
    State(state): State<crate::infrastructure::AppState>,
    _claims: crate::auth::Claims,
    axum::extract::Path(id): axum::extract::Path<i32>,
) -> impl IntoResponse {
    use crate::domain::DomainError;

    // Idempotent DELETE: return 200 OK even if book doesn't exist
    match state.book_repo.delete(id).await {
        Ok(()) | Err(DomainError::NotFound) => (
            StatusCode::OK,
            Json(json!({"message": "Book deleted successfully"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[utoipa::path(
    put,
    path = "/api/books/{id}",
    params(
        ("id" = i32, Path, description = "Book ID")
    ),
    responses(
        (status = 200, description = "Book updated successfully"),
        (status = 404, description = "Book not found"),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn update_book(
    State(state): State<crate::infrastructure::AppState>,
    _claims: crate::auth::Claims,
    axum::extract::Path(id): axum::extract::Path<i32>,
    Json(book_data): Json<Book>,
) -> impl IntoResponse {
    use crate::domain::DomainError;

    let db = state.db();
    let now = chrono::Utc::now();

    // Get current book to track owned status change
    let current_book = match state.book_repo.find_by_id(id).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Book not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let old_owned = current_book.owned.unwrap_or(true);
    let new_owned = book_data.owned.unwrap_or(old_owned);

    tracing::debug!("Updating book {} with data: {:?}", id, book_data);

    // Update book via repository
    match state.book_repo.update(id, book_data).await {
        Ok(updated_book) => {
            // Handle owned change: create or delete copies
            if new_owned != old_owned {
                use crate::models::copy::{self as copy_model, Entity as CopyEntity};
                use sea_orm::{ColumnTrait, QueryFilter};

                if new_owned {
                    // owned: false -> true: create a copy if none exists
                    let existing = CopyEntity::find()
                        .filter(copy_model::Column::BookId.eq(id))
                        .one(db)
                        .await;
                    if matches!(existing, Ok(None)) {
                        let copy = copy_model::ActiveModel {
                            book_id: Set(id),
                            library_id: Set(1),
                            status: Set("available".to_string()),
                            is_temporary: Set(false),
                            created_at: Set(now.to_rfc3339()),
                            updated_at: Set(now.to_rfc3339()),
                            ..Default::default()
                        };
                        let _ = copy.insert(db).await;
                    }
                } else {
                    // owned: true -> false: delete all copies for this book
                    let _ = CopyEntity::delete_many()
                        .filter(copy_model::Column::BookId.eq(id))
                        .exec(db)
                        .await;
                }
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Book updated successfully",
                    "book": updated_book
                })),
            )
                .into_response()
        }
        Err(DomainError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Book not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Serialize)]
pub struct TagDto {
    pub name: String,
    pub count: usize,
}

#[utoipa::path(
    get,
    path = "/api/books/tags",
    responses(
        (status = 200, description = "List all tags with counts")
    )
)]
pub async fn list_tags(
    State(db): State<DatabaseConnection>,
) -> Result<Json<Vec<TagDto>>, StatusCode> {
    use std::collections::HashMap;

    // Fetch all books
    let books = BookEntity::find()
        .all(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    // Sort by count descending, then name ascending
    tags.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));

    Ok(Json(tags))
}
#[utoipa::path(
    get,
    path = "/api/books/{id}",
    params(
        ("id" = i32, Path, description = "Book ID")
    ),
    responses(
        (status = 200, description = "Book found"),
        (status = 404, description = "Book not found"),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn get_book(
    State(state): State<crate::infrastructure::AppState>,
    axum::extract::Path(id): axum::extract::Path<i32>,
) -> impl IntoResponse {
    match state.book_repo.find_by_id(id).await {
        Ok(Some(mut book_dto)) => {
            // Add large cover URL if we have ISBN
            if let Some(isbn) = &book_dto.isbn {
                book_dto.large_cover_url = Some(format!(
                    "https://covers.openlibrary.org/b/isbn/{}-L.jpg",
                    isbn
                ));
            }
            (StatusCode::OK, Json(book_dto)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Book not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
pub struct ReorderRequest {
    pub book_ids: Vec<i32>,
}

#[utoipa::path(
    patch,
    path = "/api/books/reorder",
    request_body = ReorderRequest,
    responses(
        (status = 200, description = "Books reordered successfully"),
        (status = 500, description = "Internal server error")
    )
)]
pub async fn reorder_books(
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    Json(payload): Json<ReorderRequest>,
) -> impl IntoResponse {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, TransactionTrait};

    // We simply iterate and update shelf_position
    // For a prototype, N updates is fine. For production with thousands of books,
    // we'd use a transaction and maybe a batch update if SeaORM supports it easily,
    // or just a loop inside a transaction.

    let txn = match db.begin().await {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    for (index, book_id) in payload.book_ids.iter().enumerate() {
        // We only update if the book exists.
        // We use ActiveModel to update individual fields.
        let update_res = BookEntity::update_many()
            .col_expr(
                crate::models::book::Column::ShelfPosition,
                sea_orm::sea_query::Expr::value(index as i32),
            )
            .filter(crate::models::book::Column::Id.eq(*book_id))
            .exec(&txn)
            .await;

        if let Err(e) = update_res {
            let _ = txn.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }

    match txn.commit().await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({"message": "Books reordered successfully"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
