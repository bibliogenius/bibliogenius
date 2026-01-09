use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sea_orm::DatabaseConnection;

pub async fn lookup_book(
    State(db): State<DatabaseConnection>,
    Path(isbn): Path<String>,
) -> impl IntoResponse {
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    // Load profile config to check enabled providers
    // Default to true if profile load fails (fail open)
    let (enable_openlibrary, enable_google) =
        if let Ok(Some(profile_model)) = ProfileEntity::find_by_id(1).one(&db).await {
            let modules: Vec<String> =
                serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
            (
                !modules.contains(&"disable_fallback:openlibrary".to_string()),
                modules.contains(&"enable_google_books".to_string()),
            )
        } else {
            (true, false) // Default: OL enabled, GB disabled
        };

    // 1. Try Inventaire (single source of truth for metadata)
    if let Ok(mut inv_metadata) = crate::inventaire_client::fetch_inventaire_metadata(&isbn).await {
        // 2. Enrich with OpenLibrary cover if missing and enabled
        if inv_metadata.cover_url.is_none() && enable_openlibrary {
            inv_metadata.cover_url = crate::openlibrary::fetch_cover_url(&isbn).await;
        }
        // 3. Enrich with Google Books if still missing and enabled
        if inv_metadata.cover_url.is_none() && enable_google {
            inv_metadata.cover_url = crate::google_books::fetch_cover_url(&isbn).await;
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
    if enable_openlibrary {
        match crate::openlibrary::fetch_book_metadata(&isbn).await {
            Ok(mut metadata) => {
                // Try Google Books for cover if missing and enabled
                if metadata.cover_url.is_none() && enable_google {
                    metadata.cover_url = crate::google_books::fetch_cover_url(&isbn).await;
                }
                return (StatusCode::OK, Json(metadata)).into_response();
            }
            Err(_) => {
                // Continue to next fallback
            }
        }
    }

    // 4. Fallback to Google Books (if both Inventaire and OpenLibrary failed/disabled)
    if enable_google {
        match crate::google_books::fetch_book_metadata(&isbn).await {
            Ok(metadata) => (StatusCode::OK, Json(metadata)).into_response(),
            Err(e) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Book found nowhere: {}", e) })),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Book not found and fallbacks disabled" })),
        )
            .into_response()
    }
}
