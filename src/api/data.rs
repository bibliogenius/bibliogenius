use crate::{import, models::book};
use axum::{
    Json,
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};

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
                            Ok(_) => count += 1,
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
