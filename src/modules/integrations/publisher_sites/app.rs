//! A book's cover from its publisher's site, as the app asks for it: the
//! table shipped in the binary, the book's page found with its recipes
//! ([`find_book_page`]), then the page's cover ([`page_cover`]). One click,
//! one lookup; what keeps the sites' rules (pauses, Crawl-delays, robots.txt,
//! refusals) is shared by every call, so two clicks in a row are paced as
//! one run would be.

use std::sync::LazyLock;
use std::time::Duration;

use super::cover::{ImageError, PageCoverError, page_cover, usable_image};
use super::fetch::{PoliteFetcher, SiteFetcher};
use super::lookup::{BookQuery, Lookup, SitemapCache, find_book_page, off_runtime};
use super::page::bare_host;
use super::progress::{LookupStep, report};
use super::table::{PublisherTable, TABLE_FORMAT};
use crate::modules::integrations::web_cover::{PICK_DEADLINE, WebCover};
use crate::utils::public_fetch::parse_public_https_url;

/// The table the maintenance tool wrote, as this build ships it.
const SHIPPED_TABLE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/publisher_sites/table.json"
));

/// Pause between two requests to one host: one person looking for one book,
/// a handful of requests. A longer Crawl-delay of the site wins.
const PAUSE: Duration = Duration::from_secs(1);

/// Longest Crawl-delay waited for behind a click; a site asking more is left
/// alone, as refusing us. Gallimard's 10 s stays under it.
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(10);

/// How long a site that turned us away (429, firewall) is left alone before
/// a click asks it again.
const REFUSAL_MEMORY: Duration = Duration::from_secs(60 * 60);

static FETCHER: LazyLock<PoliteFetcher> = LazyLock::new(|| {
    PoliteFetcher::new(PAUSE)
        .with_max_crawl_delay(MAX_CRAWL_DELAY)
        .with_refusal_memory(REFUSAL_MEMORY)
        .with_bounded_memory()
});

/// The one fetcher every lookup goes through, for the life of the app.
fn shared_fetcher() -> &'static PoliteFetcher {
    &FETCHER
}

static TABLE: LazyLock<Option<PublisherTable>> = LazyLock::new(|| parse_table(SHIPPED_TABLE));

fn parse_table(json: &str) -> Option<PublisherTable> {
    match serde_json::from_str::<PublisherTable>(json) {
        Ok(table) if table.format == TABLE_FORMAT => Some(table),
        Ok(table) => {
            tracing::error!(
                "publisher site table: format {} where {TABLE_FORMAT} is expected",
                table.format
            );
            None
        }
        Err(e) => {
            tracing::error!("publisher site table unreadable: {e}");
            None
        }
    }
}

/// The table shipped with this build, read on first use. `None` when it does
/// not read: a build defect, which a test guards against.
pub fn shipped_table() -> Option<&'static PublisherTable> {
    TABLE.as_ref()
}

/// The site a lookup for the book would look on first, from the shipped
/// table alone, with no request: for the app to name it on the button that
/// starts the lookup, before the user asks. `None` for an invalid ISBN, or
/// one whose publisher the table knows no site for.
pub async fn publisher_site(isbn: String, publisher: Option<String>) -> Option<String> {
    let table = off_runtime(shipped_table).await.flatten()?;
    site_in(table, &isbn, publisher.as_deref())
}

/// [`publisher_site`] in any table, for tests.
fn site_in(table: &PublisherTable, isbn: &str, publisher: Option<&str>) -> Option<String> {
    let isbn = crate::utils::isbn::to_isbn13(isbn)?;
    table
        .site_for(&isbn, publisher)
        .map(|(site, _)| site.to_owned())
}

/// A cover found on the book's page on its publisher's site.
#[derive(Debug)]
pub struct PublisherCover {
    /// The image, its address, and the page's other images robots.txt lets
    /// us fetch, for the app's "next image".
    pub cover: WebCover,
    /// The site's key in the table (its host without `www.`).
    pub site: String,
    /// The book's page.
    pub page_url: String,
}

/// Why no cover came from the publisher's site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublisherCoverError {
    /// Not an ISBN.
    InvalidIsbn,
    /// No site in the table for the ISBN's publisher.
    NoSite,
    /// The sites answered, and none shows the book.
    NotFound,
    /// Every site turned us away.
    Refused,
    /// A site did not answer, or the search ran past its deadline: the book
    /// may well be there, to try again later.
    Unreachable,
    /// The book's page is found, but the site does not let us have its cover.
    ImageForbidden,
    /// The page shows no image of the book.
    NoImage,
    /// The page's images are gone or in a format the app cannot use.
    ImageBroken,
}

impl PublisherCoverError {
    /// A stable code for the app to map to a translated message. The codes
    /// of [`crate::modules::integrations::web_cover::WebCoverError`] are
    /// reused where the meaning is the same.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidIsbn => "invalid_isbn",
            Self::NoSite => "no_site",
            Self::NotFound => "not_found",
            Self::Refused => "blocked_by_site",
            Self::Unreachable => "network",
            Self::ImageForbidden => "image_forbidden",
            Self::NoImage | Self::ImageBroken => "no_image_found",
        }
    }
}

impl From<PageCoverError> for PublisherCoverError {
    fn from(e: PageCoverError) -> Self {
        match e {
            PageCoverError::Forbidden => Self::ImageForbidden,
            PageCoverError::Unreachable => Self::Unreachable,
            PageCoverError::NoImage => Self::NoImage,
            PageCoverError::Broken => Self::ImageBroken,
        }
    }
}

/// Look for `book`'s cover on its publisher's site, within [`PICK_DEADLINE`].
pub async fn find_publisher_cover(book: &BookQuery) -> Result<PublisherCover, PublisherCoverError> {
    // The first call parses the table (a few hundred KB): off the async
    // workers.
    let table = off_runtime(shipped_table)
        .await
        .flatten()
        .ok_or(PublisherCoverError::NoSite)?;
    tokio::time::timeout(
        PICK_DEADLINE,
        find_cover_with(shared_fetcher(), table, book),
    )
    .await
    .unwrap_or(Err(PublisherCoverError::Unreachable))
}

/// One of the candidates [`find_publisher_cover`] listed, when the user steps
/// to the next image: through the same fetcher as the lookup, so robots.txt,
/// the pause and the site's Crawl-delay hold for it as for the lookup.
pub async fn publisher_cover_image(image_url: &str) -> Result<WebCover, PublisherCoverError> {
    tokio::time::timeout(PICK_DEADLINE, cover_image_with(shared_fetcher(), image_url))
        .await
        .unwrap_or(Err(PublisherCoverError::Unreachable))
}

/// [`publisher_cover_image`] through any fetcher, for tests.
pub async fn cover_image_with(
    fetcher: &dyn SiteFetcher,
    image_url: &str,
) -> Result<WebCover, PublisherCoverError> {
    let url = parse_public_https_url(image_url).map_err(|_| PublisherCoverError::ImageBroken)?;
    report(LookupStep::Cover {
        site: bare_host(&url),
    });
    match usable_image(fetcher, &url).await {
        Ok(bytes) => Ok(WebCover {
            bytes,
            image_url: url.to_string(),
            candidates: Vec::new(),
        }),
        Err(ImageError::Forbidden) => Err(PublisherCoverError::ImageForbidden),
        Err(ImageError::Failed) => Err(PublisherCoverError::ImageBroken),
    }
}

/// [`find_publisher_cover`] through any fetcher and table, for tests.
pub async fn find_cover_with(
    fetcher: &dyn SiteFetcher,
    table: &PublisherTable,
    book: &BookQuery,
) -> Result<PublisherCover, PublisherCoverError> {
    match find_book_page(fetcher, table, book, &SitemapCache::default()).await {
        Lookup::InvalidIsbn => Err(PublisherCoverError::InvalidIsbn),
        Lookup::NoSite => Err(PublisherCoverError::NoSite),
        Lookup::NotFound {
            sites,
            refused,
            unreachable,
        } => Err(not_found(&sites, &refused, &unreachable)),
        Lookup::Found { site, page, .. } => {
            report(LookupStep::Cover { site: site.clone() });
            // `find_book_page` matched the page against the ISBN-13 form.
            let isbn = crate::utils::isbn::to_isbn13(&book.isbn).unwrap_or_default();
            let cover = page_cover(fetcher, &page, &isbn).await?;
            Ok(PublisherCover {
                cover,
                site,
                page_url: page.url.to_string(),
            })
        }
    }
}

/// Why a lookup found no page, from what the sites answered. A site that did
/// not answer proves nothing: the book is not counted missing then, whatever
/// the other sites said.
fn not_found(sites: &[String], refused: &[String], unreachable: &[String]) -> PublisherCoverError {
    if !unreachable.is_empty() {
        PublisherCoverError::Unreachable
    } else if !refused.is_empty() && refused.len() == sites.len() {
        PublisherCoverError::Refused
    } else {
        PublisherCoverError::NotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::table::{
        PrefixEntry, PrefixPublisher, ProductRecipe, SiteRecipes,
    };
    use crate::modules::integrations::publisher_sites::test_site::FakeSite;
    use crate::modules::integrations::publisher_sites::text::Apostrophe;

    const ISBN: &str = "9782000000013";
    const BOOK: &str = "https://editions.example/livre/le-grand-voyage-9782000000013";
    const IMAGE: &str = "https://editions.example/couv/9782000000013.jpg";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];

    fn table() -> PublisherTable {
        PublisherTable {
            format: TABLE_FORMAT,
            generated: "2026-11-15".into(),
            prefixes: vec![PrefixEntry {
                prefix: "978200".into(),
                publishers: vec![PrefixPublisher {
                    name: "Éditions Exemple".into(),
                    site: "editions.example".into(),
                }],
            }],
            sites: [(
                "editions.example".to_owned(),
                SiteRecipes {
                    home: "https://editions.example/".into(),
                    product: Some(ProductRecipe {
                        template: "https://editions.example/livre/{slug}-{isbn}".into(),
                        apostrophe: Apostrophe::Drop,
                        slug_ignored: false,
                    }),
                    search: None,
                    sitemap: None,
                },
            )]
            .into(),
        }
    }

    fn query(isbn: &str) -> BookQuery {
        BookQuery {
            isbn: isbn.into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            publisher: None,
        }
    }

    /// The book's page, with its ISBN and the images given.
    fn page_with(images: &[&str]) -> String {
        let tags: String = images
            .iter()
            .map(|src| format!(r#"<img src="{src}" width="400" height="600">"#))
            .collect();
        format!(
            r#"<html><head><script type="application/ld+json">{{"@type":"Book","isbn":"{ISBN}"}}</script></head><body><h1>Le Grand Voyage</h1>{tags}</body></html>"#
        )
    }

    #[tokio::test]
    async fn the_cover_comes_with_its_page_and_site() {
        let site = FakeSite::default()
            .page(BOOK, &page_with(&[IMAGE]))
            .image(IMAGE, JPEG);
        let found = find_cover_with(&site, &table(), &query(ISBN))
            .await
            .unwrap();
        assert_eq!(found.site, "editions.example");
        assert_eq!(found.page_url, BOOK);
        assert_eq!(found.cover.image_url, IMAGE);
        assert_eq!(found.cover.bytes, JPEG);
    }

    #[tokio::test]
    async fn a_hyphenated_isbn_finds_the_same_cover() {
        let site = FakeSite::default()
            .page(BOOK, &page_with(&[IMAGE]))
            .image(IMAGE, JPEG);
        let found = find_cover_with(&site, &table(), &query("978-2-00-000001-3"))
            .await
            .unwrap();
        assert_eq!(found.cover.image_url, IMAGE);
    }

    #[tokio::test]
    async fn what_the_page_lacks_is_said_in_the_app_s_terms() {
        let site = FakeSite {
            forbidden: vec!["/couv/".into()],
            ..FakeSite::default()
        }
        .page(BOOK, &page_with(&[IMAGE]));
        let err = find_cover_with(&site, &table(), &query(ISBN))
            .await
            .unwrap_err();
        assert_eq!(err, PublisherCoverError::ImageForbidden);
        assert_eq!(err.code(), "image_forbidden");

        let site = FakeSite::default().page(BOOK, &page_with(&[]));
        let err = find_cover_with(&site, &table(), &query(ISBN))
            .await
            .unwrap_err();
        assert_eq!(err, PublisherCoverError::NoImage);
        assert_eq!(err.code(), "no_image_found");
    }

    #[tokio::test]
    async fn why_no_page_was_found() {
        let check = |site: FakeSite, isbn: &'static str, want: PublisherCoverError| async move {
            let err = find_cover_with(&site, &table(), &query(isbn))
                .await
                .unwrap_err();
            assert_eq!(err, want);
        };
        check(FakeSite::default(), "123", PublisherCoverError::InvalidIsbn).await;
        check(
            FakeSite::default(),
            "9791000000015",
            PublisherCoverError::NoSite,
        )
        .await;
        check(FakeSite::default(), ISBN, PublisherCoverError::NotFound).await;
        let blocked = FakeSite {
            blocked: true,
            ..FakeSite::default()
        };
        check(blocked, ISBN, PublisherCoverError::Refused).await;
        let offline = FakeSite::default();
        offline
            .offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        check(offline, ISBN, PublisherCoverError::Unreachable).await;
    }

    #[tokio::test]
    async fn a_lookup_says_what_it_is_doing() {
        use crate::modules::integrations::publisher_sites::progress::{
            LookupStep, reporting_to, testing::recorder,
        };
        let site = FakeSite::default()
            .page(BOOK, &page_with(&[IMAGE]))
            .image(IMAGE, JPEG);
        let (reporter, steps) = recorder();
        reporting_to(reporter, find_cover_with(&site, &table(), &query(ISBN)))
            .await
            .unwrap();
        let steps = steps.lock().unwrap();
        assert!(
            matches!(
                &steps[0],
                LookupStep::ProductPage { site, attempt: 1, of } if site == "editions.example" && *of >= 1
            ),
            "{steps:?}"
        );
        assert_eq!(
            steps.last(),
            Some(&LookupStep::Cover {
                site: "editions.example".into()
            })
        );
    }

    #[tokio::test]
    async fn a_next_image_comes_through_the_same_rules() {
        let site = FakeSite {
            forbidden: vec!["/prive/".into()],
            ..FakeSite::default()
        }
        .image(IMAGE, JPEG)
        .image("https://editions.example/couv/page.jpg", b"<html></html>");

        let cover = cover_image_with(&site, IMAGE).await.unwrap();
        assert_eq!(cover.image_url, IMAGE);
        assert_eq!(cover.bytes, JPEG);
        assert!(cover.candidates.is_empty());

        let err = |url: &'static str| {
            let site = &site;
            async move { cover_image_with(site, url).await.unwrap_err() }
        };
        assert_eq!(
            err("https://editions.example/prive/1.jpg").await,
            PublisherCoverError::ImageForbidden,
            "robots.txt holds for a next image too"
        );
        assert_eq!(
            err("https://editions.example/couv/page.jpg").await,
            PublisherCoverError::ImageBroken
        );
        assert_eq!(
            err("http://127.0.0.1/couv.jpg").await,
            PublisherCoverError::ImageBroken,
            "only public https addresses"
        );
        // A silent image host: another image may do, as during the lookup.
        site.offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(err(IMAGE).await, PublisherCoverError::ImageBroken);
    }

    #[tokio::test]
    async fn a_refusal_holds_from_one_call_to_the_next() {
        // A host of its own: other tests share the app's fetcher.
        let image = "https://refused-by-test.example/couv.jpg";
        // An earlier call learnt that the site refuses us.
        shared_fetcher().refuse_for_test(&url::Url::parse(image).unwrap());
        // The next call leaves it alone. A request would have failed
        // otherwise (the host resolves nowhere): no usable image.
        assert_eq!(
            publisher_cover_image(image).await.unwrap_err(),
            PublisherCoverError::ImageForbidden
        );
    }

    #[test]
    fn the_app_s_fetcher_keeps_a_bounded_memory() {
        // It lives as long as the app: what it keeps per site must not grow
        // without limit.
        assert!(shared_fetcher().bounds_memory());
    }

    #[test]
    fn the_site_is_named_from_the_table_alone() {
        let table = table();
        assert_eq!(
            site_in(&table, "978-2-00-000001-3", None).as_deref(),
            Some("editions.example")
        );
        // Its ISBN-10 is the same book.
        assert_eq!(
            site_in(&table, "2000000010", Some("Éditions Exemple")).as_deref(),
            Some("editions.example")
        );
        assert_eq!(site_in(&table, "9791000000000", None), None, "no site");
        assert_eq!(site_in(&table, "not an isbn", None), None);
    }

    #[test]
    fn the_shipped_table_reads() {
        let table = shipped_table().expect("data/publisher_sites/table.json must parse");
        assert!(table.sites.len() > 100, "{} sites", table.sites.len());
        // Gallimard's own prefix leads to a site.
        assert!(
            !table
                .sites_for("9782070000000", Some("Gallimard"))
                .is_empty()
        );
    }

    #[test]
    fn a_table_of_another_format_is_not_used() {
        let json = serde_json::to_string(&PublisherTable {
            format: TABLE_FORMAT + 1,
            ..table()
        })
        .unwrap();
        assert!(parse_table(&json).is_none());
        assert!(parse_table("{").is_none());
    }
}
