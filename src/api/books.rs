use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{json, Value};

use crate::models::book::{ActiveModel, Entity as BookEntity};
use crate::models::Book;

#[utoipa::path(
    get,
    path = "/api/books",
    responses(
        (status = 200, description = "List all books")
    )
)]
pub async fn list_books(State(db): State<DatabaseConnection>) -> Result<Json<Value>, StatusCode> {
    use sea_orm::ModelTrait;

    let books = BookEntity::find()
        .all(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut book_dtos = Vec::new();

    for book_model in books {
        let mut book_dto = Book::from(book_model.clone());

        // Fetch authors
        if let Ok(authors) = book_model
            .find_related(crate::models::author::Entity)
            .all(&db)
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

        // Derive cover_url from ISBN if available
        if let Some(isbn) = &book_dto.isbn {
            book_dto.cover_url = Some(format!(
                "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                isbn
            ));
        }

        book_dtos.push(book_dto);
    }

    Ok(Json(json!({
        "books": book_dtos,
        "total": book_dtos.len()
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
    Json(book): Json<Book>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    let subjects_json = book
        .subjects
        .as_ref()
        .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

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
        reading_status: Set(book.reading_status.unwrap_or_else(|| "to_read".to_string())),
        created_at: Set(now.to_rfc3339()),
        updated_at: Set(now.to_rfc3339()),
        ..Default::default()
    };

    match new_book.insert(&db).await {
        Ok(model) => {
            // Handle author if provided
            if let Some(author_name) = book.author {
                use crate::models::author::{ActiveModel as AuthorActive, Entity as AuthorEntity};
                use crate::models::book_authors::ActiveModel as BookAuthorActive;
                use sea_orm::{ColumnTrait, QueryFilter};

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
                            Err(_) => {
                                // If author creation fails, continue without author
                                return (
                                    StatusCode::CREATED,
                                    Json(json!({
                                        "message": "Book created successfully (author failed)",
                                        "book": Book::from(model)
                                    })),
                                )
                                    .into_response();
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

            // Log operation
            let _ = crate::sync::log_operation(
                &db,
                "book",
                model.id,
                "INSERT",
                Some(serde_json::to_value(&model).unwrap()),
            )
            .await;

            // Create default copy
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
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    let mut book: ActiveModel = book.into();

    println!("Updating book {} with data: {:?}", id, book_data);

    // Ensure title is updated if provided, or fallback to existing?
    // Frontend is now sending title, so we can set it.
    book.title = Set(book_data.title);
    if let Some(isbn) = book_data.isbn {
        book.isbn = Set(Some(isbn));
    }
    if let Some(summary) = book_data.summary {
        book.summary = Set(Some(summary));
    }
    if let Some(publisher) = book_data.publisher {
        book.publisher = Set(Some(publisher));
    }
    if let Some(year) = book_data.publication_year {
        book.publication_year = Set(Some(year));
    }
    if let Some(status) = book_data.reading_status {
        book.reading_status = Set(status);
    }
    if let Some(finished_at) = book_data.finished_reading_at {
        book.finished_reading_at = Set(Some(finished_at));
    }
    if let Some(started_at) = book_data.started_reading_at {
        book.started_reading_at = Set(Some(started_at));
    }
    // if let Some(author) = book_data.author {
    //     // TODO: Handle author update (requires managing book_authors relation)
    //     // book.author = Set(Some(author));
    // }

    book.updated_at = Set(now.to_rfc3339());

    match book.update(&db).await {
        Ok(model) => (
            StatusCode::OK,
            Json(json!({
                "message": "Book updated successfully",
                "book": Book::from(model)
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
