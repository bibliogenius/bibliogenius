use crate::openlibrary;
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};

pub async fn lookup_book(Path(isbn): Path<String>) -> impl IntoResponse {
    match openlibrary::fetch_book_metadata(&isbn).await {
        Ok(metadata) => (StatusCode::OK, Json(metadata)).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}
