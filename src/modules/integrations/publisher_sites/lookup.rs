//! From a book to its page on its publisher's site, with the shipped table:
//! what the app will run, and what `publisher_sites measure` measures.
//!
//! A page is the book's only when it shows the book's ISBN: a title match is
//! never enough to take a cover automatically.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, OnceCell};
use tokio::time::Instant;
use url::Url;

use super::fetch::{FetchError, Page, SiteFetcher};
use super::page::{PageMatch, page_matches, result_links, sitemap_book_links, sitemap_locs};
use super::progress::{LookupStep, report};
use super::recipes::{MAX_SITEMAP_CANDIDATES, product_urls, redirected_to_book, run_search};
use super::table::{PublisherTable, RecipeKind, SiteRecipes};
use super::text::find_isbns;
use crate::modules::integrations::web_cover::{CoverCandidates, cover_candidates};
use crate::utils::isbn::{to_isbn10, to_isbn13};

/// Addresses a product template may give for one book that are tried.
/// The check uses the same bounds: a recipe it passes is one the app finds
/// books with.
pub const MAX_PRODUCT_TRIES: usize = 3;
/// Search results opened for one book.
pub const MAX_RESULT_LINKS: usize = 3;

/// What is known of the book looked for.
#[derive(Debug, Clone)]
pub struct BookQuery {
    /// ISBN-13, digits only.
    pub isbn: String,
    pub title: String,
    pub authors: String,
    /// The book's publisher field, to pick among the sites of a shared prefix.
    pub publisher: Option<String>,
}

/// How a lookup ended.
#[derive(Debug)]
pub enum Lookup {
    /// The book's page, and the site and recipe that led to it.
    Found {
        site: String,
        kind: RecipeKind,
        page: Page,
    },
    /// The book's ISBN is not one.
    InvalidIsbn,
    /// No site in the table for the book's ISBN prefix.
    NoSite,
    /// None of the sites showed the book. `refused` lists the ones that
    /// turned us away, `unreachable` the ones that did not answer at all:
    /// for those, the book may well be there, to ask again later.
    NotFound {
        sites: Vec<String>,
        refused: Vec<String>,
        unreachable: Vec<String>,
    },
}

/// Site maps already read in this run, by address: one download per site,
/// however many books are looked for there. `None` for a map that could not
/// be read; a map the network failed to bring is not kept, and is asked for
/// again with the next book.
#[derive(Default)]
pub struct SitemapCache(Mutex<HashMap<String, Arc<OnceCell<MapPages>>>>);

/// The pages a site map lists, `None` when it could not be read.
type MapPages = Option<Arc<Vec<Url>>>;

impl SitemapCache {
    async fn pages(&self, fetcher: &dyn SiteFetcher, url: &str) -> Result<MapPages, FetchError> {
        // One cell per map: two books of one site wait for one download,
        // the books of other sites do not wait at all.
        let cell = self
            .0
            .lock()
            .await
            .entry(url.to_owned())
            .or_default()
            .clone();
        cell.get_or_try_init(|| async {
            let Ok(u) = Url::parse(url) else {
                return Ok(None);
            };
            match fetcher.get(&u).await {
                Ok(map) => Ok(off_runtime(move || sitemap_locs(&map.html, &u).1)
                    .await
                    .map(Arc::new)),
                Err(e @ (FetchError::Network | FetchError::Blocked(_))) => Err(e),
                Err(_) => Ok(None),
            }
        })
        .await
        .cloned()
    }
}

/// Whether a site has answered during one lookup, to read a network error:
/// once it has, a timeout is that address failing (some servers take longer
/// than our timeout to say 404); before, it may be the whole site, or our
/// own connection, down. The same rule as the check's.
struct SiteState<'a> {
    home: &'a str,
    answered: bool,
}

impl SiteState<'_> {
    /// `Ok` when `e` only rules its address out, `Err` when it ends the
    /// lookup on this site: refused, or down. The home page is asked only
    /// on a network error met before the site answered.
    async fn tolerate(
        &mut self,
        fetcher: &dyn SiteFetcher,
        e: FetchError,
    ) -> Result<(), FetchError> {
        match e {
            FetchError::Blocked(_) => Err(e),
            FetchError::Network if !self.answered => {
                let Ok(home) = Url::parse(self.home) else {
                    return Err(e);
                };
                match fetcher.get(&home).await {
                    Ok(_) | Err(FetchError::Status(_) | FetchError::Disallowed) => {
                        self.answered = true;
                        Ok(())
                    }
                    Err(down) => Err(down),
                }
            }
            FetchError::Status(_) | FetchError::Disallowed => {
                self.answered = true;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Longest a lookup runs: behind a click, someone waits for it. The sites
/// not done with by then are reported unreachable, the book may be there.
pub const LOOKUP_DEADLINE: Duration = Duration::from_secs(60);

/// Look for `book` on the sites the table gives for its ISBN (13 or 10
/// digits, hyphens allowed), best first, each with its recipes in turn: page
/// template, search, site map. Within [`LOOKUP_DEADLINE`].
pub async fn find_book_page(
    fetcher: &dyn SiteFetcher,
    table: &PublisherTable,
    book: &BookQuery,
    maps: &SitemapCache,
) -> Lookup {
    find_book_page_within(fetcher, table, book, maps, LOOKUP_DEADLINE).await
}

/// [`find_book_page`] within `budget`. A bulk run that looks for many books
/// of one host at once gives a longer one: their lookups wait for each
/// other's turn on the host, which the app, one book at a time, does not.
pub async fn find_book_page_within(
    fetcher: &dyn SiteFetcher,
    table: &PublisherTable,
    book: &BookQuery,
    maps: &SitemapCache,
    budget: Duration,
) -> Lookup {
    let Some(isbn) = to_isbn13(&book.isbn) else {
        return Lookup::InvalidIsbn;
    };
    let book = &BookQuery {
        isbn,
        ..book.clone()
    };
    let sites = table.sites_for(&book.isbn, book.publisher.as_deref());
    if sites.is_empty() {
        return Lookup::NoSite;
    }
    let deadline = Instant::now() + budget;
    let (mut refused, mut unreachable) = (Vec::new(), Vec::new());
    for (i, (key, recipes)) in sites.iter().enumerate() {
        let Ok(answer) =
            tokio::time::timeout_at(deadline, on_site(fetcher, key, recipes, book, maps)).await
        else {
            unreachable.extend(sites[i..].iter().map(|(k, _)| (*k).to_owned()));
            break;
        };
        match answer {
            Ok(Some((kind, page))) => {
                return Lookup::Found {
                    site: (*key).to_owned(),
                    kind,
                    page,
                };
            }
            Ok(None) => {}
            Err(FetchError::Blocked(_)) => refused.push((*key).to_owned()),
            Err(_) => unreachable.push((*key).to_owned()),
        }
    }
    Lookup::NotFound {
        sites: sites.iter().map(|(k, _)| (*k).to_owned()).collect(),
        refused,
        unreachable,
    }
}

/// The book's page on one site, `Err` when the site refuses us or is down
/// (its other recipes are not tried then).
async fn on_site(
    fetcher: &dyn SiteFetcher,
    site: &str,
    recipes: &SiteRecipes,
    book: &BookQuery,
    maps: &SitemapCache,
) -> Result<Option<(RecipeKind, Page)>, FetchError> {
    let mut state = SiteState {
        home: &recipes.home,
        answered: false,
    };
    if let Some(recipe) = &recipes.product {
        let urls: Vec<String> = product_urls(recipe, &book.isbn, &book.title, &book.authors)
            .into_iter()
            .take(MAX_PRODUCT_TRIES)
            .collect();
        let of = count(urls.len());
        for (i, raw) in urls.into_iter().enumerate() {
            report(LookupStep::ProductPage {
                site: site.to_owned(),
                attempt: count(i + 1),
                of,
            });
            let Ok(url) = Url::parse(&raw) else { continue };
            if let Some(page) = book_page(fetcher, &mut state, &url, &book.isbn).await? {
                return Ok(Some((RecipeKind::Product, page)));
            }
        }
    }
    if let Some(recipe) = &recipes.search {
        let query = if recipe.by_isbn {
            &book.isbn
        } else {
            &book.title
        };
        report(LookupStep::Search {
            site: site.to_owned(),
        });
        match run_search(fetcher, recipe, query).await {
            Ok((page, asked)) => {
                state.answered = true;
                let wanted = book.clone();
                let (found, links) = off_runtime(move || {
                    if redirected_to_book(&page, &asked, &wanted.isbn)
                        && shows_isbn(&page, &wanted.isbn)
                    {
                        return (Some(page), Vec::new());
                    }
                    let links = result_links(
                        &page.html,
                        &page.url,
                        &wanted.isbn,
                        &wanted.title,
                        &wanted.authors,
                    );
                    (None, links)
                })
                .await
                .unwrap_or_default();
                if let Some(page) = found {
                    return Ok(Some((RecipeKind::Search, page)));
                }
                let links: Vec<Url> = links.into_iter().take(MAX_RESULT_LINKS).collect();
                let of = count(links.len());
                for (i, link) in links.into_iter().enumerate() {
                    report(LookupStep::SearchResult {
                        site: site.to_owned(),
                        attempt: count(i + 1),
                        of,
                    });
                    if let Some(found) = book_page(fetcher, &mut state, &link, &book.isbn).await? {
                        return Ok(Some((RecipeKind::Search, found)));
                    }
                }
            }
            Err(e) => state.tolerate(fetcher, e).await?,
        }
    }
    if let Some(recipe) = &recipes.sitemap {
        report(LookupStep::SiteMap {
            site: site.to_owned(),
        });
        let pages = match maps.pages(fetcher, &recipe.url).await {
            Ok(pages) => pages,
            Err(e) => {
                state.tolerate(fetcher, e).await?;
                None
            }
        };
        for link in pages
            .as_deref()
            .map(|p| sitemap_book_links(p, &book.isbn, &book.title))
            .unwrap_or_default()
            .into_iter()
            .take(MAX_SITEMAP_CANDIDATES)
        {
            if let Some(found) = book_page(fetcher, &mut state, &link, &book.isbn).await? {
                return Ok(Some((RecipeKind::Sitemap, found)));
            }
        }
    }
    Ok(None)
}

/// A count of tries, as steps report it.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The page at `url` when it shows `isbn`. A missing page, an address
/// robots.txt forbids, or a timeout on a site that answers only rule this
/// address out.
async fn book_page(
    fetcher: &dyn SiteFetcher,
    state: &mut SiteState<'_>,
    url: &Url,
    isbn: &str,
) -> Result<Option<Page>, FetchError> {
    match fetcher.get(url).await {
        Ok(page) => {
            state.answered = true;
            let isbn = isbn.to_owned();
            Ok(
                off_runtime(move || shows_isbn(&page, &isbn).then_some(page))
                    .await
                    .flatten(),
            )
        }
        Err(e) => {
            state.tolerate(fetcher, e).await?;
            Ok(None)
        }
    }
}

/// Whether the page shows `isbn`: the only proof a page is the book's.
pub fn shows_isbn(page: &Page, isbn: &str) -> bool {
    page_matches(&page.html, isbn, "") == Some(PageMatch::Isbn)
}

/// Run `f`, which parses a page or a site map of up to 2 MB, on the blocking
/// pool: in the app, the engine's async workers serve every other call
/// meanwhile. `None` when `f` panicked.
pub(crate) async fn off_runtime<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// The images to take `isbn`'s cover from on its `page`, to try in turn,
/// stopping at the first one the caller may not fetch (robots.txt, a site
/// that refuses us): falling back on the page's other images then would
/// take a button or a banner. The order:
/// 1. any image named after the book's ISBN, declared or not;
/// 2. the first image the page declares (JSON-LD, `og:image`);
/// 3. its `<img>` tags marked as the book's (`couv_livre`), which stand in
///    for a declared image that is gone;
/// 4. the other images it declares (often a share card or a related title);
/// 5. on a page that declares none of its own, its other `<img>` tags.
///
/// An image named after another ISBN is another book's (a page often shows
/// related titles), and the site's own images are left out.
pub fn cover_images(page: &Page, isbn: &str) -> Vec<Url> {
    let CoverCandidates {
        declared,
        book_tags,
        tags,
    } = cover_candidates(&page.html, &page.url);
    let named = |u: &Url| names_isbn(u, isbn);
    let mut out: Vec<Url> = declared
        .iter()
        .chain(&book_tags)
        .chain(&tags)
        .filter(|u| named(u))
        .cloned()
        .collect();
    let other = |urls: Vec<Url>| -> Vec<Url> {
        urls.into_iter()
            .filter(|u| !named(u) && !names_another_book(u, isbn) && !is_site_image(u))
            .collect()
    };
    // A page that declares a stand-in for a missing cover says it has none:
    // its tags are a newsletter banner or a menu, not the book.
    if !declared.is_empty() && declared.iter().all(|u| !named(u) && is_stand_in(u)) {
        return out;
    }
    // A page declares an image of its own when one is not the site's.
    let declares = declared.iter().any(|u| named(u) || !is_site_image(u));
    if declares {
        let mut declared = other(declared).into_iter();
        out.extend(declared.next());
        out.extend(other(book_tags));
        out.extend(declared);
    } else {
        out.extend(other(book_tags));
        out.extend(other(tags));
    }
    out
}

/// Whether the image's address carries `isbn`, in either length.
fn names_isbn(url: &Url, isbn: &str) -> bool {
    let address = url.as_str();
    address.contains(isbn) || to_isbn10(isbn).is_some_and(|i| address.contains(&i))
}

/// Whether the image's address carries an ISBN-13 other than `isbn`.
fn names_another_book(url: &Url, isbn: &str) -> bool {
    find_isbns(url.path()).iter().any(|other| other != isbn)
}

/// Words of an image's file name that mark a stand-in for a missing cover.
/// Not "default", which PrestaShop puts in every image size
/// (`123-large_default.jpg`), nor "missing" or "blank", which titles carry.
const STAND_IN_WORDS: [&str; 6] = [
    "placeholder",
    "manquant",
    "manquante",
    "indisponible",
    "noimage",
    "nocover",
];

/// Whether an image belongs to the site rather than to a book: kept in the
/// site's theme (`/wp-content/themes/`, `/themes/seuil/`), where a site
/// stores its logo, buttons and default share image, or among its icons
/// (`/generic/icon/cb.svg`), or named as a stand-in
/// for a missing cover (`couverture-manquante.png`).
fn is_site_image(url: &Url) -> bool {
    let segments: Vec<String> = url
        .path_segments()
        .map(|s| s.map(str::to_lowercase).collect())
        .unwrap_or_default();
    if segments.iter().rev().skip(1).any(|s| {
        matches!(
            s.as_str(),
            "themes" | "theme" | "icon" | "icons" | "picto" | "pictos"
        )
    }) {
        return true;
    }
    is_stand_in(url)
}

/// Whether an image is a stand-in for a missing cover, by its name
/// (`couverture-manquante.png`, `nocover.jpg`) or its folder
/// (`/images/placeholders/…/cover.png`).
fn is_stand_in(url: &Url) -> bool {
    if url
        .path_segments()
        .is_some_and(|mut s| s.any(|seg| seg.to_lowercase().contains("placeholder")))
    {
        return true;
    }
    let file = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("")
        .to_lowercase();
    let squeezed: String = file.chars().filter(char::is_ascii_alphanumeric).collect();
    file.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| STAND_IN_WORDS.contains(&w))
        || ["noimage", "nocover"].iter().any(|w| squeezed.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::table::{
        PrefixEntry, PrefixPublisher, ProductRecipe, SitemapRecipe, TABLE_FORMAT,
    };
    use crate::modules::integrations::publisher_sites::test_site::{FakeSite, book_page};
    use crate::modules::integrations::publisher_sites::text::Apostrophe;

    const ISBN: &str = "9782000000013";

    fn table(recipes: SiteRecipes) -> PublisherTable {
        PublisherTable {
            format: TABLE_FORMAT,
            generated: "2026-11-14".into(),
            prefixes: vec![PrefixEntry {
                prefix: "978200".into(),
                publishers: vec![PrefixPublisher {
                    name: "Éditions Exemple".into(),
                    site: "editions.example".into(),
                }],
            }],
            sites: [("editions.example".to_owned(), recipes)].into(),
        }
    }

    fn query() -> BookQuery {
        BookQuery {
            isbn: ISBN.into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            publisher: None,
        }
    }

    fn product(template: &str) -> SiteRecipes {
        SiteRecipes {
            home: "https://editions.example/".into(),
            product: Some(ProductRecipe {
                template: template.into(),
                apostrophe: Apostrophe::Drop,
                slug_ignored: false,
            }),
            search: None,
            sitemap: None,
        }
    }

    #[tokio::test]
    async fn finds_the_page_that_shows_the_isbn() {
        let book = "https://editions.example/livre/le-grand-voyage-9782000000013";
        let site = FakeSite::default().page(book, &book_page(ISBN, "Le Grand Voyage"));
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        match find_book_page(&site, &t, &query(), &SitemapCache::default()).await {
            Lookup::Found { site, kind, page } => {
                assert_eq!(site, "editions.example");
                assert_eq!(kind, RecipeKind::Product);
                assert_eq!(page.url.as_str(), book);
            }
            other => panic!("{other:?}"),
        }
        // Another prefix: no site to ask.
        let other = BookQuery {
            isbn: "9791000000015".into(),
            ..query()
        };
        assert!(matches!(
            find_book_page(&site, &t, &other, &SitemapCache::default()).await,
            Lookup::NoSite
        ));
    }

    #[tokio::test]
    async fn an_isbn_10_or_with_hyphens_is_looked_for_as_its_isbn_13() {
        let book = "https://editions.example/livre/le-grand-voyage-9782000000013";
        let site = FakeSite::default().page(book, &book_page(ISBN, "Le Grand Voyage"));
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        for isbn in ["978-2-00-000001-3", "2000000010"] {
            let asked = BookQuery {
                isbn: isbn.into(),
                ..query()
            };
            assert!(
                matches!(
                    find_book_page(&site, &t, &asked, &SitemapCache::default()).await,
                    Lookup::Found { .. }
                ),
                "{isbn}"
            );
        }
        let bad = BookQuery {
            isbn: "12345".into(),
            ..query()
        };
        assert!(matches!(
            find_book_page(&site, &t, &bad, &SitemapCache::default()).await,
            Lookup::InvalidIsbn
        ));
    }

    #[tokio::test]
    async fn a_lookup_past_its_deadline_reports_the_site_unreachable() {
        let site = FakeSite {
            latency: Duration::from_millis(300),
            ..FakeSite::default()
        };
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        let lookup = find_book_page_within(
            &site,
            &t,
            &query(),
            &SitemapCache::default(),
            Duration::from_millis(50),
        )
        .await;
        match lookup {
            Lookup::NotFound { unreachable, .. } => {
                assert_eq!(unreachable, ["editions.example"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_page_with_the_title_but_not_the_isbn_is_not_taken() {
        // Another edition of the same title, under its own ISBN.
        let book = "https://editions.example/livre/le-grand-voyage-9782000000013";
        let site = FakeSite::default().page(book, &book_page("9782000000028", "Le Grand Voyage"));
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        assert!(matches!(
            find_book_page(&site, &t, &query(), &SitemapCache::default()).await,
            Lookup::NotFound { .. }
        ));
    }

    #[tokio::test]
    async fn a_site_map_is_read_once_for_every_book() {
        let map = "https://editions.example/sitemap.xml";
        let book = "https://editions.example/romans/le-grand-voyage/";
        let site = FakeSite::default()
            .page(
                map,
                &format!("<urlset><url><loc>{book}</loc></url></urlset>"),
            )
            .page(book, &book_page(ISBN, "Le Grand Voyage"));
        let t = table(SiteRecipes {
            home: "https://editions.example/".into(),
            product: None,
            search: None,
            sitemap: Some(SitemapRecipe { url: map.into() }),
        });
        let maps = SitemapCache::default();
        for _ in 0..2 {
            assert!(matches!(
                find_book_page(&site, &t, &query(), &maps).await,
                Lookup::Found {
                    kind: RecipeKind::Sitemap,
                    ..
                }
            ));
        }
        let asked = site.asked.lock().unwrap();
        assert_eq!(asked.iter().filter(|u| u.as_str() == map).count(), 1);
    }

    #[tokio::test]
    async fn a_site_that_refuses_us_is_reported() {
        let site = FakeSite {
            blocked: true,
            ..FakeSite::default()
        };
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        match find_book_page(&site, &t, &query(), &SitemapCache::default()).await {
            Lookup::NotFound { refused, .. } => assert_eq!(refused, ["editions.example"]),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_site_that_does_not_answer_is_unreachable_not_missing_the_book() {
        let site = FakeSite::default();
        site.offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        match find_book_page(&site, &t, &query(), &SitemapCache::default()).await {
            Lookup::NotFound {
                refused,
                unreachable,
                ..
            } => {
                assert!(refused.is_empty());
                assert_eq!(unreachable, ["editions.example"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_timeout_on_a_site_that_answers_only_rules_the_address_out() {
        // The home page answers, the book's address hangs: the book is not
        // there as far as we can tell, the site is not down.
        let site = FakeSite {
            slow: vec!["/livre/".into()],
            ..FakeSite::default()
        }
        .page(
            "https://editions.example/",
            "<html><body>Accueil</body></html>",
        );
        let t = table(product("https://editions.example/livre/{slug}-{isbn}"));
        match find_book_page(&site, &t, &query(), &SitemapCache::default()).await {
            Lookup::NotFound { unreachable, .. } => assert!(unreachable.is_empty()),
            other => panic!("{other:?}"),
        }
        let asked = site.asked.lock().unwrap();
        // The home page, once, to tell the two apart.
        assert_eq!(
            asked
                .iter()
                .filter(|u| u.as_str() == "https://editions.example/")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_site_map_the_network_failed_to_bring_is_asked_for_again() {
        let map = "https://editions.example/sitemap.xml";
        let book = "https://editions.example/romans/le-grand-voyage/";
        let site = FakeSite::default()
            .page(
                map,
                &format!("<urlset><url><loc>{book}</loc></url></urlset>"),
            )
            .page(book, &book_page(ISBN, "Le Grand Voyage"));
        let t = table(SiteRecipes {
            home: "https://editions.example/".into(),
            product: None,
            search: None,
            sitemap: Some(SitemapRecipe { url: map.into() }),
        });
        let maps = SitemapCache::default();
        site.offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            find_book_page(&site, &t, &query(), &maps).await,
            Lookup::NotFound { ref unreachable, .. } if unreachable == &["editions.example"]
        ));
        site.offline
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            find_book_page(&site, &t, &query(), &maps).await,
            Lookup::Found {
                kind: RecipeKind::Sitemap,
                ..
            }
        ));
    }

    #[test]
    fn a_page_that_declares_its_image_gives_no_other() {
        let page = |html: &str| Page {
            url: Url::parse("https://e.example/ouvrage/x/9782000000013").unwrap(),
            html: html.into(),
            truncated: false,
        };
        // The cover on another host, then a close button among the tags.
        let declared = page(
            r#"<head><meta property="og:image" content="https://images.example/9782000000013.jpg"></head>
            <body><img src="/assets/fermer.jpg" width="400" height="400"></body>"#,
        );
        let images: Vec<String> = cover_images(&declared, ISBN)
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(images, ["https://images.example/9782000000013.jpg"]);
        // Nothing declared: the tags are all there is.
        let bare = page(r#"<body><img src="/couv/9782000000013.jpg" width="400"></body>"#);
        assert_eq!(cover_images(&bare, ISBN).len(), 1);
    }

    #[test]
    fn the_site_s_own_images_are_never_a_cover() {
        let page = |html: &str| Page {
            url: Url::parse("https://e.example/catalogue/la-danse/").unwrap(),
            html: html.into(),
            truncated: false,
        };
        // The theme's share image declared for every page: the tags decide.
        let shared = page(
            r#"<head><meta property="og:image" content="https://e.example/wp-content/themes/fat/img/og-image.jpg"></head>
            <body><img class="book-introduction__image" src="https://e.example/wp-content/uploads/2024/05/couv.png"></body>"#,
        );
        let images: Vec<String> = cover_images(&shared, ISBN)
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            images,
            ["https://e.example/wp-content/uploads/2024/05/couv.png"]
        );
        // A stand-in for a missing cover, a theme button: nothing left.
        let stand_in = page(
            r#"<body><img src="https://e.example/assets/img/couverture-manquante.png">
            <img src="https://e.example/themes/seuil/assets/images/fermer.jpg"></body>"#,
        );
        assert!(cover_images(&stand_in, ISBN).is_empty());
        // A page that declares a stand-in has no cover: its tags are not one.
        let none = page(
            r#"<head><meta property="og:image" content="https://e.example/bundles/site/images/nocover.jpg"></head>
            <body><img src="https://e.example/storage/images/popin-newsletter/Popin-newsletter.png"></body>"#,
        );
        assert!(cover_images(&none, ISBN).is_empty());
        // A stand-in kept in a folder named so, whatever its own name.
        let folder = page(
            r#"<body><img src="https://e.example/images/placeholders/maison/cover.png"></body>"#,
        );
        assert!(cover_images(&folder, ISBN).is_empty());
        // PrestaShop names every size "default": not a stand-in.
        let shop =
            page(r#"<body><img src="https://e.example/123-large_default/la-danse.jpg"></body>"#);
        assert_eq!(cover_images(&shop, ISBN).len(), 1);
        // A cover kept with the site's assets but named after the ISBN stays.
        let named = page(
            r#"<body><img src="https://e.example/assets/img/couvertures/9782000000013.jpg"></body>"#,
        );
        assert_eq!(cover_images(&named, ISBN).len(), 1);
    }

    #[test]
    fn the_image_named_after_the_book_wins_and_another_book_s_is_dropped() {
        let page = Page {
            url: Url::parse("https://e.example/catalogue/x").unwrap(),
            // The page declares a related title's cover; its own is a tag.
            html: r#"<head><meta property="og:image" content="https://e.example/cover/9782000000028.jpg"></head>
                <body><img src="https://e.example/portrait/cover/9782000000013.jpg"></body>"#
                .into(),
            truncated: false,
        };
        let images: Vec<String> = cover_images(&page, ISBN)
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            images,
            ["https://e.example/portrait/cover/9782000000013.jpg"]
        );
    }

    #[test]
    fn a_tag_marked_as_the_cover_comes_before_the_other_declared_images() {
        // Two og:image: the cover (gone, but that is for the caller to see)
        // and a share card; the page shows the cover as a marked tag.
        let page = Page {
            url: Url::parse("https://e.example/Eloge-du-petit-matin").unwrap(),
            html: r#"<head><meta property="og:image" content="https://e.example/logo/eloge_hd.jpg">
                <meta property="og:image" content="https://e.example/local/cache/L1200xH630/card.jpg"></head>
                <body><img src="https://e.example/local/cache/eloge_hd-80023.jpg" class="couv_livre">
                <img src="https://e.example/generic/icon/cb.svg" class="produit-paiement">
                <img src="https://e.example/local/cache/auteur.png"></body>"#
                .into(),
            truncated: false,
        };
        let images: Vec<String> = cover_images(&page, ISBN)
            .iter()
            .map(Url::to_string)
            .collect();
        assert_eq!(
            images,
            [
                "https://e.example/logo/eloge_hd.jpg",
                "https://e.example/local/cache/eloge_hd-80023.jpg",
                "https://e.example/local/cache/L1200xH630/card.jpg",
            ]
        );
    }
}
