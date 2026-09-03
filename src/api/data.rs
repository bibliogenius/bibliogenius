use crate::{import, models::book};
use axum::{
    Json,
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

pub async fn import_file(
    State(db): State<DatabaseConnection>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        if field.name() == Some("file") {
            let data = field.bytes().await.unwrap_or_default();
            match import::parse_import_file(&data) {
                Ok(books) => {
                    let mut count = 0;
                    let mut errors = Vec::new();
                    for req in books {
                        let now = chrono::Utc::now();
                        // Check for existing book by ISBN
                        let existing = if let Some(ref isbn) = req.isbn {
                            book::Entity::find()
                                .filter(book::Column::Isbn.eq(isbn))
                                .one(&db)
                                .await
                                .ok()
                                .flatten()
                        } else {
                            None
                        };
                        if existing.is_some() {
                            count += 1; // Already exists, skip
                            continue;
                        }
                        let new_book = book::ActiveModel {
                            title: Set(req.title.clone()),
                            isbn: Set(req.isbn),
                            summary: Set(None),
                            publisher: Set(req.publisher),
                            publication_year: Set(req.publication_year),
                            created_at: Set(now.to_rfc3339()),
                            updated_at: Set(now.to_rfc3339()),
                            ..Default::default()
                        };
                        match new_book.insert(&db).await {
                            Ok(created) => {
                                // The source named an author: link it, so an
                                // imported shelf is browsable by author like
                                // any hand-entered book.
                                if let Some(ref author_name) = req.author
                                    && let Err(e) =
                                        crate::services::book_service::create_or_link_author(
                                            &db,
                                            &created.id,
                                            author_name,
                                        )
                                        .await
                                {
                                    // The book is in: an unlinked author is a
                                    // gap in the shelf, not a failed import, so
                                    // it is reported here rather than counted
                                    // as an error the user cannot act on.
                                    tracing::warn!(
                                        "Import: could not link author for {}: {:?}",
                                        created.id,
                                        e
                                    );
                                }
                                count += 1;
                            }
                            Err(e) => errors.push(format!("{}: {}", req.title, e)),
                        }
                    }
                    return (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "imported": count,
                            "errors": if errors.is_empty() { None } else { Some(errors) }
                        })),
                    )
                        .into_response();
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": e })),
                    )
                        .into_response();
                }
            }
        }
    }
    (StatusCode::BAD_REQUEST, "No file uploaded").into_response()
}
