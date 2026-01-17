use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::DatabaseConnection;

#[derive(serde::Deserialize)]
pub struct LookupParams {
    pub lang: Option<String>,
}

pub async fn lookup_book(
    State(db): State<DatabaseConnection>,
    Path(isbn): Path<String>,
    axum::extract::Query(params): axum::extract::Query<LookupParams>,
) -> impl IntoResponse {
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    // Load profile config to check enabled providers
    let (enable_openlibrary, enable_google, enable_inventaire, enable_bnf) =
        match ProfileEntity::find_by_id(1).one(&db).await {
            Ok(Some(profile_model)) => {
                let modules: Vec<String> =
                    serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
                (
                    !modules.contains(&"disable_fallback:openlibrary".to_string()),
                    modules.contains(&"enable_google_books".to_string()),
                    !modules.contains(&"disable_fallback:inventaire".to_string()),
                    !modules.contains(&"disable_fallback:bnf".to_string()),
                )
            }
            _ => (true, false, true, true),
        };

    // Pre-check: For French ISBNs, prioritize BNF (more reliable for French books)
    let clean_isbn = isbn.replace('-', "");
    let is_french_isbn = clean_isbn.starts_with("9782") || clean_isbn.starts_with("97910");
    let user_lang_is_french = params.lang.as_deref().unwrap_or("").starts_with("fr");

    // For French ISBNs, try BNF first (better coverage for French publishers)
    if enable_bnf && is_french_isbn {
        tracing::debug!("ISBN {} is French, trying BNF first", isbn);
        match crate::modules::integrations::bnf::lookup_bnf_isbn(&clean_isbn).await {
            Ok(Some(bnf_book)) => {
                tracing::info!("BNF found book for ISBN {}: {}", isbn, bnf_book.title);
                let authors = bnf_book
                    .author
                    .map(|name| {
                        vec![crate::inventaire_client::AuthorMetadata {
                            name,
                            birth_year: None,
                            death_year: None,
                            image_url: None,
                            bio: None,
                        }]
                    })
                    .unwrap_or_default();

                let metadata = crate::openlibrary::BookMetadata {
                    title: bnf_book.title,
                    authors,
                    publisher: bnf_book.publisher,
                    publication_year: bnf_book.publication_year.map(|y| y.to_string()),
                    cover_url: bnf_book.cover_url,
                    summary: bnf_book.description,
                };
                return (StatusCode::OK, Json(metadata)).into_response();
            }
            Ok(None) => {
                tracing::debug!("BNF returned no result for ISBN {}", isbn);
            }
            Err(e) => {
                tracing::warn!("BNF lookup failed for {}: {}", isbn, e);
            }
        }
    }

    // 1. Try Inventaire
    if enable_inventaire {
        tracing::debug!("Trying Inventaire for ISBN {}", isbn);
        match crate::inventaire_client::fetch_inventaire_metadata(&isbn).await {
            Ok(mut inv_metadata) => {
                tracing::info!(
                    "Inventaire found book for ISBN {}: {}",
                    isbn,
                    inv_metadata.title
                );
                // Enrich with cover from other sources if missing
                if inv_metadata.cover_url.is_none() && enable_openlibrary {
                    inv_metadata.cover_url = crate::openlibrary::fetch_cover_url(&isbn).await;
                }
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
            Err(e) => {
                tracing::debug!("Inventaire lookup failed for {}: {}", isbn, e);
            }
        }
    }

    // 2. Fallback to OpenLibrary
    if enable_openlibrary {
        tracing::debug!("Trying OpenLibrary for ISBN {}", isbn);
        match crate::openlibrary::fetch_book_metadata(&isbn).await {
            Ok(mut metadata) => {
                tracing::info!(
                    "OpenLibrary found book for ISBN {}: {}",
                    isbn,
                    metadata.title
                );
                if metadata.cover_url.is_none() && enable_google {
                    metadata.cover_url = crate::google_books::fetch_cover_url(&isbn).await;
                }
                return (StatusCode::OK, Json(metadata)).into_response();
            }
            Err(e) => {
                tracing::debug!("OpenLibrary lookup failed for {}: {}", isbn, e);
            }
        }
    }

    // 3. Fallback to BNF for non-French ISBNs (French ISBNs already tried first)
    // Try BNF if user language is French but ISBN is not French
    if enable_bnf && user_lang_is_french && !is_french_isbn {
        tracing::debug!("Trying BNF (user lang French) for ISBN {}", isbn);
        match crate::modules::integrations::bnf::lookup_bnf_isbn(&clean_isbn).await {
            Ok(Some(bnf_book)) => {
                tracing::info!("BNF found book for ISBN {}: {}", isbn, bnf_book.title);
                let authors = bnf_book
                    .author
                    .map(|name| {
                        vec![crate::inventaire_client::AuthorMetadata {
                            name,
                            birth_year: None,
                            death_year: None,
                            image_url: None,
                            bio: None,
                        }]
                    })
                    .unwrap_or_default();

                let metadata = crate::openlibrary::BookMetadata {
                    title: bnf_book.title,
                    authors,
                    publisher: bnf_book.publisher,
                    publication_year: bnf_book.publication_year.map(|y| y.to_string()),
                    cover_url: bnf_book.cover_url,
                    summary: bnf_book.description,
                };
                return (StatusCode::OK, Json(metadata)).into_response();
            }
            Ok(None) => {
                tracing::debug!("BNF returned no result for ISBN {}", isbn);
            }
            Err(e) => {
                tracing::warn!("BNF lookup failed for {}: {}", isbn, e);
            }
        }
    }

    // 4. Fallback to Google Books
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
