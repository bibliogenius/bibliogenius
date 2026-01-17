#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{Value, json};

use crate::models::Book;
use crate::models::book::{ActiveModel, Entity as BookEntity};

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
    State(db): State<DatabaseConnection>,
    axum::extract::Query(filter): axum::extract::Query<BookFilter>,
) -> Result<Json<Value>, StatusCode> {
    use sea_orm::{ColumnTrait, Condition, ModelTrait, PaginatorTrait, QueryFilter, QueryOrder};

    tracing::info!(
        "List books request - Filters: status={:?}, title={:?}, tag={:?}",
        filter.status,
        filter.title,
        filter.tag
    );

    let mut query = BookEntity::find();

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

    // Tag filter (searching in JSON subjects array via simple text match for compatibility)
    if let Some(tag_query) = &filter.tag
        && !tag_query.is_empty()
    {
        query = query.filter(crate::models::book::Column::Subjects.contains(tag_query));
    }

    if let Some(q) = &filter.q
        && !q.is_empty()
    {
        let cond = Condition::any()
            .add(crate::models::book::Column::Title.contains(q))
            .add(crate::models::book::Column::Isbn.contains(q))
            .add(crate::models::book::Column::Subjects.contains(q));
        query = query.filter(cond);
    }

    // ... (existing code, ensure imports are correct or just use crate::models::book::Column etc)

    // --- SORTING (Database Level) ---
    match filter.sort.as_deref() {
        Some("title_asc") => {
            query = query.order_by_asc(crate::models::book::Column::Title);
        }
        Some("title_desc") => {
            query = query.order_by_desc(crate::models::book::Column::Title);
        }
        Some("recent") => {
            query = query.order_by_desc(crate::models::book::Column::CreatedAt);
        }
        // "author_asc" is hard to do at DB level without joins.
        // For now, if author sorting is requested with pagination,
        // we might NOT sort by author correctly across pages (it will sort by ID/Shelf then paginate).
        // Improvements: Implement join-based sort or complex query.
        _ => {
            query = query.order_by_asc(crate::models::book::Column::ShelfPosition);
        }
    }

    // --- PAGINATION & FETCHING ---
    let (books, total_count) = if let Some(limit) = filter.limit {
        let page = filter.page.unwrap_or(0);
        let paginator = query.paginate(&db, limit);
        let total = paginator.num_items().await.unwrap_or(0);
        let items = paginator.fetch_page(page).await.unwrap_or(vec![]);
        (items, total)
    } else {
        let items = query
            .all(&db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let total = items.len() as u64;
        (items, total)
    };

    tracing::info!(
        "DB query returned {} books (Total: {})",
        books.len(),
        total_count
    );

    let mut book_dtos = Vec::new();

    for book_model in books {
        let mut book_dto = Book::from(book_model.clone());

        // Fetch authors
        if let Ok(authors) = book_model
            .find_related(crate::models::author::Entity)
            .all(&db)
            .await
            && !authors.is_empty()
        {
            let author_names: Vec<String> = authors.into_iter().map(|a| a.name).collect();
            book_dto.author = Some(author_names.join(", ")); // Backward compat
            book_dto.authors = Some(author_names); // New array field
        }

        // Derive cover_url
        if let Some(isbn) = &book_dto.isbn {
            book_dto.cover_url = Some(format!(
                "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                isbn
            ));
        }

        // Note: We skip the "DEFENSIVE" in-memory filtering here for performance.
        // We rely on DB filters. If DB filters are correct, we don't need double-check.
        // Also, pagination makes in-memory filtering impossible (as we only have a slice).

        book_dtos.push(book_dto);
    }

    // Apply in-memory sorting ONLY if we fetched everything (no limit) OR if it's a sort we couldn't do in DB?
    // Actually, if we paginate, we CANNOT sort in memory.
    // So for "author_asc", if paginated, it will just be "unsorted" (or shelf/ID sorted).
    // The user must accept this limitation for now or we implement complex DB sort later.
    // However, if NO LIMIT is provided (Local Library), we can still do in-memory sort to support 'author_asc'.

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
        "total": total_count
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
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    Json(book): Json<Book>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    let subjects_json = book
        .subjects
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

    // Determine owned status: false for 'wanting', true otherwise
    let reading_status = book
        .reading_status
        .clone()
        .unwrap_or_else(|| "to_read".to_string());
    let owned = book.owned.unwrap_or_else(|| reading_status != "wanting");

    let new_book = ActiveModel {
        title: Set(book.title.clone()),
        isbn: Set(book.isbn),
        summary: Set(book.summary),
        publisher: Set(book.publisher),
        publication_year: Set(book.publication_year),
        dewey_decimal: Set(book.dewey_decimal),
        lcc: Set(book.lcc),
        subjects: Set(subjects_json),
        marc_record: Set(book.marc_record),
        cataloguing_notes: Set(book.cataloguing_notes),
        source_data: Set(book.source_data),
        reading_status: Set(reading_status),
        started_reading_at: Set(book.started_reading_at.flatten()),
        finished_reading_at: Set(book.finished_reading_at.flatten()),
        owned: Set(owned),
        price: Set(book.price),
        digital_formats: Set(book
            .digital_formats
            .map(|s| serde_json::to_string(&s).unwrap_or_else(|_| "[]".to_string()))),
        created_at: Set(now.to_rfc3339()),
        updated_at: Set(now.to_rfc3339()),
        ..Default::default()
    };

    match new_book.insert(&db).await {
        Ok(model) => {
            // Handle authors - support both single author and authors array
            // Preference: use authors array if provided, fall back to single author
            let author_names: Vec<String> = if let Some(ref authors) = book.authors {
                authors.clone()
            } else if let Some(ref author) = book.author {
                // Split comma-separated for backward compat
                author
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                Vec::new()
            };

            if !author_names.is_empty() {
                use crate::models::author::{ActiveModel as AuthorActive, Entity as AuthorEntity};
                use crate::models::book_authors::ActiveModel as BookAuthorActive;
                use sea_orm::{ColumnTrait, QueryFilter};

                for author_name in author_names {
                    // Find or create author
                    let author = match AuthorEntity::find()
                        .filter(crate::models::author::Column::Name.eq(&author_name))
                        .one(&db)
                        .await
                    {
                        Ok(Some(existing)) => existing,
                        _ => {
                            // Create new author
                            let new_author = AuthorActive {
                                name: Set(author_name),
                                created_at: Set(now.to_rfc3339()),
                                updated_at: Set(now.to_rfc3339()),
                                ..Default::default()
                            };
                            match new_author.insert(&db).await {
                                Ok(created) => created,
                                Err(e) => {
                                    tracing::warn!("Failed to create author: {}", e);
                                    continue; // Skip this author but continue with others
                                }
                            }
                        }
                    };

                    // Create book-author relation
                    let book_author = BookAuthorActive {
                        book_id: Set(model.id),
                        author_id: Set(author.id),
                        ..Default::default()
                    };
                    let _ = book_author.insert(&db).await;
                }
            }

            // Log operation
            let _ = crate::sync::log_operation(
                &db,
                "book",
                model.id,
                "INSERT",
                Some(serde_json::to_value(&model).unwrap()),
            )
            .await;

            // Create default copy only if owned
            if model.owned {
                let copy = crate::models::copy::ActiveModel {
                    book_id: Set(model.id),
                    library_id: Set(1), // Default library ID
                    status: Set("available".to_string()),
                    is_temporary: Set(false),
                    created_at: Set(now.to_rfc3339()),
                    updated_at: Set(now.to_rfc3339()),
                    ..Default::default()
                };
                let _ = copy.insert(&db).await;
            }

            (
                StatusCode::CREATED,
                Json(json!({
                    "message": "Book created successfully",
                    "book": Book::from(model)
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
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    axum::extract::Path(id): axum::extract::Path<i32>,
) -> impl IntoResponse {
    match BookEntity::delete_by_id(id).exec(&db).await {
        Ok(_) => (
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
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    axum::extract::Path(id): axum::extract::Path<i32>,
    Json(book_data): Json<Book>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    // Find the book first
    let book = match BookEntity::find_by_id(id).one(&db).await {
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

    let mut book: ActiveModel = book.into();

    println!("Updating book {} with data: {:?}", id, book_data);

    // Ensure title is updated if provided, or fallback to existing?
    // Frontend is now sending title, so we can set it.
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

    // Handle attributes/tags update
    if let Some(subjects) = book_data.subjects {
        let subjects_json = serde_json::to_string(&subjects).unwrap_or_else(|_| "[]".to_string());
        book.subjects = Set(Some(subjects_json));
    }

    // Handle user_rating update
    book.user_rating = Set(book_data.user_rating);

    // Handle owned field - track if we need to create/delete copies
    let old_owned = match &book.owned {
        sea_orm::ActiveValue::Unchanged(v) => *v,
        sea_orm::ActiveValue::Set(v) => *v,
        _ => true,
    };
    let new_owned = book_data.owned.unwrap_or(old_owned);
    if new_owned != old_owned {
        book.owned = Set(new_owned);
    }

    book.price = Set(book_data.price);

    // Handle digital_formats update
    if let Some(formats) = book_data.digital_formats {
        let formats_json = serde_json::to_string(&formats).unwrap_or_else(|_| "[]".to_string());
        book.digital_formats = Set(Some(formats_json));
    }

    book.updated_at = Set(now.to_rfc3339());

    match book.update(&db).await {
        Ok(model) => {
            // Handle owned change: create or delete copies
            if new_owned != old_owned {
                use crate::models::copy::{self as copy_model, Entity as CopyEntity};
                use sea_orm::{ColumnTrait, QueryFilter};

                if new_owned {
                    // owned: false -> true: create a copy if none exists
                    let existing = CopyEntity::find()
                        .filter(copy_model::Column::BookId.eq(id))
                        .one(&db)
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
                        let _ = copy.insert(&db).await;
                    }
                } else {
                    // owned: true -> false: delete all copies for this book
                    let _ = CopyEntity::delete_many()
                        .filter(copy_model::Column::BookId.eq(id))
                        .exec(&db)
                        .await;
                }
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Book updated successfully",
                    "book": Book::from(model)
                })),
            )
                .into_response()
        }
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
    State(db): State<DatabaseConnection>,
    axum::extract::Path(id): axum::extract::Path<i32>,
) -> impl IntoResponse {
    use sea_orm::{EntityTrait, ModelTrait};

    match BookEntity::find_by_id(id).one(&db).await {
        Ok(Some(book_model)) => {
            let mut book_dto = Book::from(book_model.clone());

            // Fetch authors
            if let Ok(authors) = book_model
                .find_related(crate::models::author::Entity)
                .all(&db)
                .await
            {
                if !authors.is_empty() {
                    let author_names: Vec<String> = authors.into_iter().map(|a| a.name).collect();
                    book_dto.author = Some(author_names.join(", ")); // Backward compat
                    book_dto.authors = Some(author_names); // New array field
                }
            }

            // Derive cover_url
            if let Some(isbn) = &book_dto.isbn {
                book_dto.cover_url = Some(format!(
                    "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                    isbn
                ));
                // Add large cover URL
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
