use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};

pub async fn lookup_book(Path(isbn): Path<String>) -> impl IntoResponse {
    // 1. Try Inventaire (single source of truth for metadata)
    if let Ok(mut inv_metadata) = crate::inventaire_client::fetch_inventaire_metadata(&isbn).await {
        // 2. Enrich with OpenLibrary cover if missing
        if inv_metadata.cover_url.is_none() {
            inv_metadata.cover_url = crate::openlibrary::fetch_cover_url(&isbn).await;
        }

        let metadata = crate::openlibrary::BookMetadata {
            title: inv_metadata.title,
            authors: inv_metadata.authors,
            publisher: inv_metadata.publisher,
            publication_year: inv_metadata.publication_year,
            cover_url: inv_metadata.cover_url,
            summary: inv_metadata.summary,
        };
        return (StatusCode::OK, Json(metadata)).into_response();
    }

    // 3. Fallback to OpenLibrary (only if Inventaire completely fails)
    match crate::openlibrary::fetch_book_metadata(&isbn).await {
        Ok(metadata) => (StatusCode::OK, Json(metadata)).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}
