// Cover search and enrichment for books.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

/// Enrich books that have an ISBN but no cover by checking external sources.
/// Runs in background, returns the count of covers found and persisted.
pub async fn enrich_missing_covers() -> Result<i32, String> {
    let db = db().ok_or("Database not initialized")?;
    let book_repo =
        crate::infrastructure::repositories::book_repository::SeaOrmBookRepository::new(db.clone());
    crate::services::book_service::enrich_missing_covers(db, &book_repo)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Search for a cover URL for a single ISBN from external sources.
pub async fn search_cover_for_book(isbn: String) -> Result<Option<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_service::search_cover_for_book(db, &isbn)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Search for a cover URL by title with author verification.
/// Used as a fallback when ISBN-based search returns nothing.
/// Returns a cover only if the result author matches the given author.
pub async fn search_cover_by_title(
    title: String,
    author: Option<String>,
    enable_google: Option<bool>,
) -> Result<Option<String>, String> {
    let gb_api_key = load_google_books_api_key().await;
    crate::services::book_service::search_cover_by_title(
        &title,
        author.as_deref(),
        enable_google.unwrap_or(false),
        gb_api_key.as_deref(),
    )
    .await
    .map_err(|e| format!("{:?}", e))
}

/// A cover candidate from an external source, for the multi-cover picker.
///
/// `language` is the edition's language code when the source states it. The
/// picker shows it: choosing between covers of several editions of the same
/// work is a decision the reader can only make if the editions are told apart,
/// and the language is the difference that shows.
#[frb(dart_metadata=("freezed"))]
pub struct FrbCoverCandidate {
    pub url: String,
    pub source: String,
    pub language: Option<String>,
}

impl From<crate::services::book_service::CoverCandidate> for FrbCoverCandidate {
    fn from(c: crate::services::book_service::CoverCandidate) -> Self {
        FrbCoverCandidate {
            url: c.url,
            source: c.source,
            language: c.language,
        }
    }
}

/// What one source answered during a cover search.
///
/// `state` is one of `found`, `empty`, `skipped`, `unavailable`. `detail` carries
/// the reason behind `unavailable` (HTTP status, transport error, or `quota`).
/// Flattened to strings rather than mirrored as an enum, matching the rest of
/// this bridge.
#[frb(dart_metadata=("freezed"))]
pub struct FrbCoverSourceStatus {
    pub source: String,
    pub state: String,
    pub detail: Option<String>,
}

impl From<crate::services::book_service::CoverSourceStatus> for FrbCoverSourceStatus {
    fn from(s: crate::services::book_service::CoverSourceStatus) -> Self {
        use crate::services::book_service::CoverSourceOutcome as O;
        let (state, detail) = match s.outcome {
            O::Found(_) => ("found", None),
            O::Empty => ("empty", None),
            O::Skipped => ("skipped", None),
            O::Unavailable(reason) => ("unavailable", Some(reason)),
        };
        FrbCoverSourceStatus {
            source: s.source,
            state: state.to_string(),
            detail,
        }
    }
}

/// Cover candidates plus what each source answered, so the picker can tell the
/// user that a source was down instead of claiming no cover exists.
#[frb(dart_metadata=("freezed"))]
pub struct FrbCoverSearchResult {
    pub candidates: Vec<FrbCoverCandidate>,
    pub sources: Vec<FrbCoverSourceStatus>,
}

impl From<crate::services::book_service::CoverSearchReport> for FrbCoverSearchResult {
    fn from(r: crate::services::book_service::CoverSearchReport) -> Self {
        FrbCoverSearchResult {
            candidates: r.candidates.into_iter().map(Into::into).collect(),
            sources: r.sources.into_iter().map(Into::into).collect(),
        }
    }
}

/// Search ALL enabled cover sources in parallel for a given ISBN.
/// Returns the candidates for the picker carousel and each source's answer.
pub async fn search_all_covers_for_book(isbn: String) -> Result<FrbCoverSearchResult, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_service::search_all_covers_for_book(db, &isbn)
        .await
        .map(FrbCoverSearchResult::from)
        .map_err(|e| format!("{:?}", e))
}

/// Search ALL enabled sources by title in parallel for the cover picker.
pub async fn search_all_covers_by_title(
    title: String,
    author: Option<String>,
    enable_google: Option<bool>,
) -> Result<FrbCoverSearchResult, String> {
    let db = db().ok_or("Database not initialized")?;
    let gb_api_key = load_google_books_api_key().await;
    crate::services::book_service::search_all_covers_by_title(
        db,
        &title,
        author.as_deref(),
        enable_google.unwrap_or(false),
        gb_api_key.as_deref(),
    )
    .await
    .map(FrbCoverSearchResult::from)
    .map_err(|e| format!("{:?}", e))
}
