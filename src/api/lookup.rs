use crate::openlibrary;
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};

pub async fn lookup_book(Path(isbn): Path<String>) -> impl IntoResponse {
    // 1. Try Inventaire
    if let Ok(inv_metadata) = crate::inventaire_client::fetch_inventaire_metadata(&isbn).await {
        let metadata = crate::openlibrary::BookMetadata {
            title: inv_metadata.title,
            authors: inv_metadata.authors,
            publisher: inv_metadata.publisher,
            publication_year: inv_metadata.publication_year,
            cover_url: inv_metadata.cover_url,
        };
        return (StatusCode::OK, Json(metadata)).into_response();
    }

    // 2. Fallback to OpenLibrary
    match crate::openlibrary::fetch_book_metadata(&isbn).await {
        Ok(metadata) => (StatusCode::OK, Json(metadata)).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}
