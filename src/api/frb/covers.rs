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

/// A cover found on a web page: the raw image bytes, the image's address and,
/// for a page, every usable candidate best first (`image_url` included), for
/// the app to step through.
pub struct FrbWebCover {
    pub bytes: Vec<u8>,
    pub image_url: String,
    pub candidates: Vec<String>,
}

impl From<crate::modules::integrations::web_cover::WebCover> for FrbWebCover {
    fn from(c: crate::modules::integrations::web_cover::WebCover) -> Self {
        FrbWebCover {
            bytes: c.bytes,
            image_url: c.image_url,
            candidates: c.candidates,
        }
    }
}

/// Fetch the cover of the book shown at `url`, a publisher's book page or the
/// image itself. `isbn`, when the book has one, ranks the images named after
/// it first. The bytes are returned unprocessed: the app runs them through its
/// cover photo pipeline before saving.
///
/// On failure the error is a stable code (`invalid_url`, `blocked_address`,
/// `network`, `too_many_redirects`, `blocked_by_site`, `http_status`,
/// `too_large`, `no_image_found`, `unsupported_image`) for the app to
/// translate.
///
/// FFI only, with no Axum route on purpose: an HTTP endpoint that fetches an
/// arbitrary URL would let any peer make this device issue requests.
pub async fn fetch_cover_from_web_page(
    url: String,
    isbn: Option<String>,
) -> Result<FrbWebCover, String> {
    crate::modules::integrations::web_cover::fetch_cover_from_page(&url, isbn.as_deref())
        .await
        .map(Into::into)
        .map_err(|e| e.code().to_string())
}

/// Fetch one of the candidates [`fetch_cover_from_web_page`] listed, when the
/// user steps to the next image. Same checks and error codes; FFI only for the
/// same reason.
pub async fn fetch_web_cover_image(image_url: String) -> Result<FrbWebCover, String> {
    crate::modules::integrations::web_cover::fetch_cover_image(&image_url)
        .await
        .map(Into::into)
        .map_err(|e| e.code().to_string())
}

/// Record that the cover file just saved for `book_id` came from `image_url`,
/// so it can later be offered to Open Library by address. Device-local.
pub async fn record_web_cover_origin(book_id: String, image_url: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    let dir = covers_dir().ok_or("Covers directory not registered")?;
    crate::services::web_cover_service::record_cover_origin(db, dir, &book_id, &image_url).await
}

/// A cover found on the book's page on its publisher's site.
///
/// `candidates` are the page's images robots.txt lets us fetch, best first,
/// `image_url` included: "next image" fetches them with
/// [`fetch_publisher_cover_image`], under the lookup's rules. `page_url` is
/// the book's page, for the user to open; `site` the publisher's site, as the
/// table names it. For a next image, only `bytes` and `image_url` are set.
pub struct FrbPublisherCover {
    pub bytes: Vec<u8>,
    pub image_url: String,
    pub candidates: Vec<String>,
    pub page_url: String,
    pub site: String,
}

impl From<crate::modules::integrations::publisher_sites::app::PublisherCover>
    for FrbPublisherCover
{
    fn from(c: crate::modules::integrations::publisher_sites::app::PublisherCover) -> Self {
        FrbPublisherCover {
            bytes: c.cover.bytes,
            image_url: c.cover.image_url,
            candidates: c.cover.candidates,
            page_url: c.page_url,
            site: c.site,
        }
    }
}

/// One step of a publisher site lookup, for the app to say what is being
/// done while the user waits.
///
/// `kind` is one of `rules` (reading robots.txt), `product_page` (trying the
/// book's page at an address built from its ISBN: `attempt` of at most `of`),
/// `search` (the site's own search), `search_result` (opening a result:
/// `attempt` of at most `of`), `site_map`, `waiting` (the site asks for a
/// pause: `seconds` left), `queued` (waiting for another request of ours to
/// the site to end) and `cover` (fetching a cover image). `site` is the host without `www.`. Flattened to a string, like
/// [`FrbCoverSourceStatus`].
pub struct FrbPublisherLookupStep {
    pub kind: String,
    pub site: String,
    pub attempt: u32,
    pub of: u32,
    pub seconds: u32,
}

impl From<crate::modules::integrations::publisher_sites::progress::LookupStep>
    for FrbPublisherLookupStep
{
    fn from(step: crate::modules::integrations::publisher_sites::progress::LookupStep) -> Self {
        use crate::modules::integrations::publisher_sites::progress::LookupStep as S;
        let (kind, site, attempt, of, seconds) = match step {
            S::Rules { site } => ("rules", site, 0, 0, 0),
            S::ProductPage { site, attempt, of } => ("product_page", site, attempt, of, 0),
            S::Search { site } => ("search", site, 0, 0, 0),
            S::SearchResult { site, attempt, of } => ("search_result", site, attempt, of, 0),
            S::SiteMap { site } => ("site_map", site, 0, 0, 0),
            S::Waiting { site, seconds } => ("waiting", site, 0, 0, seconds),
            S::Queued { site } => ("queued", site, 0, 0, 0),
            S::Cover { site } => ("cover", site, 0, 0, 0),
        };
        FrbPublisherLookupStep {
            kind: kind.to_owned(),
            site,
            attempt,
            of,
            seconds,
        }
    }
}

/// What [`find_cover_on_publisher_site`] sends: steps while it works, then
/// exactly one `cover` or one `error`, the last event of the stream.
pub struct FrbPublisherLookupEvent {
    pub step: Option<FrbPublisherLookupStep>,
    pub cover: Option<FrbPublisherCover>,
    pub error: Option<String>,
}

/// The publisher's site [`find_cover_on_publisher_site`] would look on first
/// for this book (`gallimard.fr`), read from the table shipped in the engine,
/// with no request: for the app to name the site on the button that starts
/// the lookup. `None` when the ISBN is invalid or the table knows no site for
/// its publisher; `publisher` is the book's publisher field, as for the
/// lookup.
pub async fn publisher_site_for(isbn: String, publisher: Option<String>) -> Option<String> {
    crate::modules::integrations::publisher_sites::app::publisher_site(isbn, publisher).await
}

/// Look for the book's cover on its publisher's own site, with the publisher
/// site table shipped in the engine: the ISBN prefix gives the site, the
/// site's recipe its page for the book, which must state the ISBN. robots.txt
/// is honoured for the page and the image alike. The bytes are returned
/// unprocessed, like [`fetch_cover_from_web_page`]'s.
///
/// `publisher` is the book's publisher field, to pick among the publishers of
/// a shared prefix; `authors` helps the recipes that need the author's name.
///
/// A stream: the lookup's steps (a site asking 10 s between two requests
/// makes it last half a minute), then the cover, or an error with a stable
/// code: `invalid_isbn`, `no_site` (no site known for this publisher),
/// `not_found` (the site answered, without the book), `blocked_by_site`,
/// `network` (a site did not answer: worth trying later), `image_forbidden`
/// (the site does not let us have the cover), `no_image_found`. The outcome
/// is an event rather than this function's result, which the generated Dart
/// side does not await.
///
/// FFI only, with no Axum route on purpose: a route would let any peer make
/// this device send requests to publisher sites.
pub async fn find_cover_on_publisher_site(
    isbn: String,
    title: String,
    authors: String,
    publisher: Option<String>,
    sink: crate::frb_generated::StreamSink<FrbPublisherLookupEvent>,
) -> Result<(), String> {
    use crate::modules::integrations::publisher_sites::{app, lookup::BookQuery};
    let book = BookQuery {
        isbn,
        title,
        authors,
        publisher,
    };
    stream_publisher_lookup(sink, async {
        app::find_publisher_cover(&book).await.map(Into::into)
    })
    .await;
    Ok(())
}

/// Fetch one of the `candidates` [`find_cover_on_publisher_site`] listed,
/// when the user steps to the next image. Unlike [`fetch_web_cover_image`],
/// it goes through the lookup's fetcher: robots.txt, the pause between two
/// requests to a host and the site's Crawl-delay hold for it too, which can
/// take as long, so it is a stream of steps as well. Same events and error
/// codes as the lookup; the cover carries `bytes` and `image_url` only. FFI
/// only, for the same reason.
pub async fn fetch_publisher_cover_image(
    image_url: String,
    sink: crate::frb_generated::StreamSink<FrbPublisherLookupEvent>,
) -> Result<(), String> {
    use crate::modules::integrations::publisher_sites::app;
    stream_publisher_lookup(sink, async {
        app::publisher_cover_image(&image_url)
            .await
            .map(|cover| FrbPublisherCover {
                bytes: cover.bytes,
                image_url: cover.image_url,
                candidates: Vec::new(),
                page_url: String::new(),
                site: String::new(),
            })
    })
    .await;
    Ok(())
}

/// Send `lookup`'s steps to `sink`, then its outcome as the last event. The
/// outcome is an event rather than the FFI function's result, which the
/// generated Dart side does not await. Once the app stops listening (the
/// dialog closed), the lookup stops at its next step instead of holding its
/// turn on the host for nobody.
async fn stream_publisher_lookup(
    sink: crate::frb_generated::StreamSink<FrbPublisherLookupEvent>,
    lookup: impl std::future::Future<
        Output = Result<
            FrbPublisherCover,
            crate::modules::integrations::publisher_sites::app::PublisherCoverError,
        >,
    >,
) {
    use crate::modules::integrations::publisher_sites::progress;
    // Shared rather than cloned: a clone of the sink needs its events to be
    // Clone, and a cover carries the image's bytes.
    let sink = std::sync::Arc::new(sink);
    let steps = sink.clone();
    let reporter: progress::Reporter = std::sync::Arc::new(move |step| {
        steps
            .add(FrbPublisherLookupEvent {
                step: Some(step.into()),
                cover: None,
                error: None,
            })
            .is_ok()
    });
    let Some(outcome) = progress::until_abandoned(reporter, lookup).await else {
        return;
    };
    let _ = sink.add(match outcome {
        Ok(cover) => FrbPublisherLookupEvent {
            step: None,
            cover: Some(cover),
            error: None,
        },
        Err(e) => FrbPublisherLookupEvent {
            step: None,
            cover: None,
            error: Some(e.code().to_string()),
        },
    });
}
