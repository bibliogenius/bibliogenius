//! A book's cover from its publisher's site, as the app asks for it: the
//! table shipped in the binary, the book's page found with its recipes
//! ([`find_book_page`]), then the page's cover ([`page_cover`]). Nothing is
//! kept between two calls: one click, one lookup.

use std::sync::LazyLock;
use std::time::Duration;

use super::cover::{PageCoverError, page_cover};
use super::fetch::{PoliteFetcher, SiteFetcher};
use super::lookup::{BookQuery, Lookup, SitemapCache, find_book_page, off_runtime};
use super::table::{PublisherTable, TABLE_FORMAT};
use crate::modules::integrations::web_cover::{PICK_DEADLINE, WebCover};

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
    let fetcher = PoliteFetcher::new(PAUSE).with_max_crawl_delay(MAX_CRAWL_DELAY);
    tokio::time::timeout(PICK_DEADLINE, find_cover_with(&fetcher, table, book))
        .await
        .unwrap_or(Err(PublisherCoverError::Unreachable))
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
