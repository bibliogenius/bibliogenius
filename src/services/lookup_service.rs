//! Lookup Service — multi-source ISBN metadata resolution
//!
//! Extracted from `api/lookup.rs` so it can be called from both
//! the HTTP handler and FFI (metadata refresh feature).

use sea_orm::DatabaseConnection;

use crate::openlibrary::BookMetadata;

/// Resolve book metadata by ISBN, querying multiple sources in priority order.
///
/// Source priority depends on ISBN origin and user language:
/// - French ISBNs (978-2…): BNF SPARQL → SUDOC → BNF SRU → Inventaire → OpenLibrary → Google
/// - Others: Inventaire → OpenLibrary → BNF (if user lang is French) → Google
///
/// Returns `Ok(None)` when no source has data for this ISBN.
pub async fn lookup_metadata_by_isbn(
    db: &DatabaseConnection,
    isbn: &str,
    user_lang: Option<&str>,
) -> Result<Option<BookMetadata>, String> {
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    // Load profile config to check enabled providers and API keys
    let (enable_openlibrary, enable_google, enable_inventaire, enable_bnf, google_api_key) =
        match ProfileEntity::find_by_id(1).one(db).await {
            Ok(Some(profile_model)) => {
                let modules: Vec<String> =
                    serde_json::from_str(&profile_model.enabled_modules).unwrap_or_default();
                let api_keys: std::collections::HashMap<String, String> = profile_model
                    .api_keys
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                (
                    !modules.contains(&"disable_fallback:openlibrary".to_string()),
                    modules.contains(&"enable_google_books".to_string()),
                    !modules.contains(&"disable_fallback:inventaire".to_string()),
                    !modules.contains(&"disable_fallback:bnf".to_string()),
                    api_keys.get("google_books").cloned(),
                )
            }
            _ => (true, false, true, true, None),
        };

    let clean_isbn = isbn.replace('-', "");
    let is_french_isbn = clean_isbn.starts_with("9782") || clean_isbn.starts_with("97910");
    let user_lang_is_french = user_lang.unwrap_or("").starts_with("fr");

    // For French ISBNs, try BNF first (better coverage for French publishers)
    if enable_bnf && is_french_isbn {
        if let Some(m) = try_bnf_sparql(
            &clean_isbn,
            isbn,
            enable_openlibrary,
            enable_google,
            google_api_key.as_deref(),
        )
        .await
        {
            return Ok(Some(m));
        }
        if let Some(m) = try_sudoc(
            &clean_isbn,
            isbn,
            enable_openlibrary,
            enable_google,
            google_api_key.as_deref(),
        )
        .await
        {
            return Ok(Some(m));
        }
        if let Some(m) = try_bnf_sru(
            &clean_isbn,
            isbn,
            enable_openlibrary,
            enable_google,
            google_api_key.as_deref(),
        )
        .await
        {
            return Ok(Some(m));
        }
    }

    // 1. Try Inventaire
    if enable_inventaire
        && let Some(m) = try_inventaire(
            isbn,
            enable_openlibrary,
            enable_google,
            google_api_key.as_deref(),
        )
        .await
    {
        return Ok(Some(m));
    }

    // 2. Fallback to OpenLibrary
    if enable_openlibrary
        && let Some(m) = try_openlibrary(isbn, enable_google, google_api_key.as_deref()).await
    {
        return Ok(Some(m));
    }

    // 3. BNF for non-French ISBNs if user language is French
    if enable_bnf
        && user_lang_is_french
        && !is_french_isbn
        && let Some(m) = try_bnf_sparql(
            &clean_isbn,
            isbn,
            enable_openlibrary,
            enable_google,
            google_api_key.as_deref(),
        )
        .await
    {
        return Ok(Some(m));
    }

    // 4. Fallback to Google Books
    if enable_google {
        match crate::google_books::fetch_book_metadata(isbn, google_api_key.as_deref()).await {
            Ok(metadata) => return Ok(Some(metadata)),
            Err(e) => {
                tracing::debug!("Google Books lookup failed for {}: {}", isbn, e);
            }
        }
    }

    Ok(None)
}

// ─── Internal helpers ───────────────────────────────────────────────

fn make_authors_from_name(name: Option<String>) -> Vec<crate::inventaire_client::AuthorMetadata> {
    name.map(|n| {
        vec![crate::inventaire_client::AuthorMetadata {
            name: n,
            birth_year: None,
            death_year: None,
            image_url: None,
            bio: None,
        }]
    })
    .unwrap_or_default()
}

async fn enrich_cover(
    isbn: &str,
    cover_url: Option<String>,
    enable_openlibrary: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<String> {
    if cover_url.is_some() {
        return cover_url;
    }

    // Try cover sources in parallel to maximize chances of finding one
    match (enable_openlibrary, enable_google) {
        (true, true) => {
            let (ol_result, gb_result) = tokio::join!(
                crate::openlibrary::fetch_cover_url(isbn),
                crate::google_books::fetch_cover_url(isbn, google_api_key),
            );
            ol_result.or(gb_result)
        }
        (true, false) => crate::openlibrary::fetch_cover_url(isbn).await,
        (false, true) => crate::google_books::fetch_cover_url(isbn, google_api_key).await,
        (false, false) => None,
    }
}

async fn try_bnf_sparql(
    clean_isbn: &str,
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying BNF SPARQL for ISBN {}", isbn);
    match crate::modules::integrations::bnf::lookup_bnf_isbn(clean_isbn).await {
        Ok(Some(bnf_book)) => {
            tracing::info!("BNF found book for ISBN {}: {}", isbn, bnf_book.title);
            let cover_url = enrich_cover(
                isbn,
                bnf_book.cover_url,
                enable_openlibrary,
                enable_google,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: bnf_book.title,
                authors: make_authors_from_name(bnf_book.author),
                publisher: bnf_book.publisher,
                publication_year: bnf_book.publication_year.map(|y| y.to_string()),
                cover_url,
                summary: bnf_book.description,
            })
        }
        Ok(None) => {
            tracing::debug!("BNF returned no result for ISBN {}", isbn);
            None
        }
        Err(e) => {
            tracing::warn!("BNF lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_sudoc(
    clean_isbn: &str,
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying SUDOC for French ISBN {}", isbn);
    match crate::modules::integrations::sudoc::fetch_by_isbn(clean_isbn).await {
        Ok(sudoc_book) => {
            tracing::info!("SUDOC found book for ISBN {}: {}", isbn, sudoc_book.title);
            let cover_url = enrich_cover(
                isbn,
                None,
                enable_openlibrary,
                enable_google,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: sudoc_book.title,
                authors: make_authors_from_name(sudoc_book.author),
                publisher: sudoc_book.publisher,
                publication_year: sudoc_book.publication_year.map(|y| y.to_string()),
                cover_url,
                summary: None,
            })
        }
        Err(e) => {
            tracing::debug!("SUDOC lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_bnf_sru(
    clean_isbn: &str,
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying BNF SRU for French ISBN {}", isbn);
    match crate::modules::integrations::bnf::lookup_bnf_sru(clean_isbn).await {
        Ok(Some(bnf_book)) => {
            tracing::info!("BNF SRU found book for ISBN {}: {}", isbn, bnf_book.title);
            let cover_url = enrich_cover(
                isbn,
                bnf_book.cover_url,
                enable_openlibrary,
                enable_google,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: bnf_book.title,
                authors: make_authors_from_name(bnf_book.author),
                publisher: bnf_book.publisher,
                publication_year: bnf_book.publication_year.map(|y| y.to_string()),
                cover_url,
                summary: bnf_book.description,
            })
        }
        Ok(None) => {
            tracing::debug!("BNF SRU returned no result for ISBN {}", isbn);
            None
        }
        Err(e) => {
            tracing::debug!("BNF SRU lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_inventaire(
    isbn: &str,
    enable_openlibrary: bool,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying Inventaire for ISBN {}", isbn);
    match crate::inventaire_client::fetch_inventaire_metadata(isbn).await {
        Ok(inv_metadata) => {
            tracing::info!(
                "Inventaire found book for ISBN {}: {}",
                isbn,
                inv_metadata.title
            );
            let cover_url = enrich_cover(
                isbn,
                inv_metadata.cover_url,
                enable_openlibrary,
                enable_google,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                title: inv_metadata.title,
                authors: inv_metadata.authors,
                publisher: inv_metadata.publisher,
                publication_year: inv_metadata.publication_year,
                cover_url,
                summary: inv_metadata.summary,
            })
        }
        Err(e) => {
            tracing::debug!("Inventaire lookup failed for {}: {}", isbn, e);
            None
        }
    }
}

async fn try_openlibrary(
    isbn: &str,
    enable_google: bool,
    google_api_key: Option<&str>,
) -> Option<BookMetadata> {
    tracing::debug!("Trying OpenLibrary for ISBN {}", isbn);
    match crate::openlibrary::fetch_book_metadata(isbn).await {
        Ok(metadata) => {
            tracing::info!(
                "OpenLibrary found book for ISBN {}: {}",
                isbn,
                metadata.title
            );
            let cover_url = enrich_cover(
                isbn,
                metadata.cover_url,
                false,
                enable_google,
                google_api_key,
            )
            .await;
            Some(BookMetadata {
                cover_url,
                ..metadata
            })
        }
        Err(e) => {
            tracing::debug!("OpenLibrary lookup failed for {}: {}", isbn, e);
            None
        }
    }
}
