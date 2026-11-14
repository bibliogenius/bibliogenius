//! The maintenance probe: find, for one publisher site, a recipe that leads
//! from an ISBN to the book's page, and prove it on books of the site.
//!
//! 1. Witnesses: books the site currently sells, taken from links of its home
//!    page (ISBN, title and author read on their own page), plus any given by
//!    hand.
//! 2. Product pattern: the witnesses' addresses with `{isbn}`, `{slug}`,
//!    `{author}` put back, kept when it rebuilds them; a deliberately wrong
//!    slug tells whether the server reads only the ISBN.
//! 3. Search: the search forms of the home page and of a few usual pages
//!    (GET kept as is, POST submitted once and its redirect kept), then usual
//!    addresses. Each is run on a nonsense query first: a page that already
//!    links the witnesses (the home page under another name) proves nothing,
//!    and an answer identical for every query means results drawn by
//!    JavaScript.
//!
//! The probe goes through [`SiteFetcher`], so robots.txt, pacing and
//! blocking are the fetcher's business, and tests run on an in-memory site.

use url::Url;

use super::fetch::{FetchError, Page, PostAnswer, SiteFetcher};
use super::page::{
    PageMatch, announced_books, book_facts, catalogue_links, other_links, page_matches,
    result_links, search_forms, single_book_facts, sitemap_book_links, sitemap_locs,
    unannounced_pages, witness_links,
};
use super::table::{
    ProbeStatus, ProductCheck, ProductRecipe, SITEMAP_RECIPES, SearchCheck, SearchRecipe,
    SiteProbe, SitemapCheck, SitemapRecipe, Witness,
};
use super::text::{Apostrophe, author_variants, slugify, title_variants};

/// Witnesses looked for on the home page.
const WANTED_WITNESSES: usize = 3;
/// Links announcing a book opened on a page to find them.
const MAX_WITNESS_LINKS: usize = WANTED_WITNESSES + 4;
/// Places of a page where a link nothing announces is opened.
const SAMPLE_SPOTS: usize = 3;
/// Such links opened per list of links, neighbours of a book included.
const MAX_SAMPLED_LINKS: usize = 7;
/// Older books taken in the site map besides the recent ones.
const BACKLIST_WITNESSES: usize = 2;
/// Pages of the site map opened to find them.
const MAX_BACKLIST_LINKS: usize = 4;
/// Site maps read for one site, an index counting as one.
const MAX_SITEMAPS_READ: usize = 3;
/// Pages a site map recipe may open for one book: the best matches of
/// [`sitemap_book_links`].
pub const MAX_SITEMAP_CANDIDATES: usize = 2;
/// Witnesses a site map must lead to, to be a recipe.
const MIN_SITEMAP_WITNESSES: usize = 2;
/// Words in the address of the map that lists a site's books.
const BOOK_SITEMAP_WORDS: [&str; 8] = [
    "product",
    "produit",
    "livre",
    "book",
    "ouvrage",
    "catalogue",
    "publication",
    "titre",
];
/// Pages, besides the home page, whose forms are read.
const SEARCH_PAGES: [&str; 3] = ["/catalogue", "/recherche", "/search"];
/// Search forms tried per page: each costs a few requests, and a page full
/// of forms (filters, sorting) must not multiply them.
const MAX_FORMS_PER_PAGE: usize = 5;
/// WordPress's search, and the same search under its other address.
const WORDPRESS_SEARCH: &str = "/?s={q}";
const WORDPRESS_SEARCH_ALIAS: &str = "/search/{q}";
/// Usual search addresses tried after the site's forms, the ones that most
/// often work first (measured on some 450 French publisher sites, where
/// WordPress's alone made more than half of the guessed recipes). The alias
/// is skipped when WordPress's search already finds witnesses.
const GUESSES: [&str; 9] = [
    WORDPRESS_SEARCH,
    "/recherche?s={q}",
    "/search?q={q}",
    "/recherche?q={q}",
    "/search?query={q}",
    "/recherche/{q}",
    "/catalogue?s%5Bq%5D={q}",
    "/catalogsearch/result/?q={q}",
    WORDPRESS_SEARCH_ALIAS,
];
/// The control query: no book has it in its title.
pub const CONTROL_QUERY: &str = "zzqxwv";
/// A real result page, even empty, carries the site's menus: less than this
/// without a link is an error message.
const MIN_RESULT_PAGE_BYTES: usize = 2000;

/// Probe `home`. `extra` witnesses (given by hand) are tested along with
/// those of the home page. `today` dates the result.
pub async fn probe_site(
    fetcher: &dyn SiteFetcher,
    home: &Url,
    extra: &[Witness],
    today: &str,
) -> SiteProbe {
    let mut out = SiteProbe {
        home: home.to_string(),
        status: ProbeStatus::Offline,
        probed_at: today.to_owned(),
        note: None,
        witnesses: Vec::new(),
        product: None,
        search: None,
        tried: Vec::new(),
        book_sitemap: None,
        sitemap: None,
    };
    match run(fetcher, home, extra, &mut out).await {
        Ok(status) => out.status = status,
        Err(e) => {
            out.status = match e {
                FetchError::Blocked(_) => ProbeStatus::Blocked,
                FetchError::Disallowed => ProbeStatus::RobotsForbid,
                _ => ProbeStatus::Offline,
            };
            out.note = Some(describe(&e));
        }
    }
    out
}

fn describe(e: &FetchError) -> String {
    match e {
        FetchError::Blocked(why) => format!("blocked: {why}"),
        FetchError::Disallowed => "forbidden by robots.txt".into(),
        FetchError::Status(s) => format!("HTTP {s}"),
        FetchError::Network => "unreachable".into(),
        FetchError::Refused => "address refused (not public https, or too large)".into(),
    }
}

/// The probe proper. An `Err` is a reason to stop probing the site (blocked,
/// or its home page unreachable); what was learnt so far stays in `out`.
async fn run(
    fetcher: &dyn SiteFetcher,
    home: &Url,
    extra: &[Witness],
    out: &mut SiteProbe,
) -> Result<ProbeStatus, FetchError> {
    let home_page = fetcher.get(home).await?;
    let hunted = hunt_witnesses(fetcher, &home_page).await?;
    out.book_sitemap = hunted.book_sitemap;
    let book_pages = hunted.book_pages;
    let mut witnesses = hunted.found;
    for w in extra {
        if !witnesses.iter().any(|x| x.isbn == w.isbn) {
            witnesses.push(w.clone());
        }
    }
    out.witnesses = witnesses.clone();
    if witnesses.is_empty() {
        return Ok(ProbeStatus::NoWitness);
    }

    let with_url: Vec<Witness> = witnesses
        .iter()
        .filter(|w| w.url.is_some())
        .cloned()
        .collect();
    out.product = probe_product(fetcher, &with_url)
        .await?
        .filter(|p| p.checked_on >= 2 || p.recipe.slug_ignored);

    let (best, tried) = probe_search(fetcher, &home_page, &witnesses).await?;
    out.tried = tried;
    out.search = best;

    if SITEMAP_RECIPES && let Some(map) = &out.book_sitemap {
        out.sitemap = sitemap_check(map, &book_pages, &with_url);
    }

    if out.product.is_some() || out.search.is_some() || out.sitemap.is_some() {
        return Ok(ProbeStatus::Ok);
    }
    Ok(
        if !out.tried.is_empty() && out.tried.iter().all(|t| t.broken) {
            ProbeStatus::SearchDown
        } else if out.tried.iter().any(|t| t.js_results) {
            ProbeStatus::JsResults
        } else {
            ProbeStatus::NoRecipe
        },
    )
}

/// The site map as a recipe: kept when it leads to at least
/// [`MIN_SITEMAP_WITNESSES`] of the witnesses, and to two thirds of them.
/// No request: the witnesses' pages were read when they were found.
fn sitemap_check(map: &str, pages: &[Url], witnesses: &[Witness]) -> Option<SitemapCheck> {
    let found = witnesses
        .iter()
        .filter(|w| {
            let Some(page) = w.url.as_deref() else {
                return false;
            };
            sitemap_book_links(pages, &w.isbn, &w.title)
                .iter()
                .take(MAX_SITEMAP_CANDIDATES)
                .any(|c| page_key(c.as_str()) == page_key(page))
        })
        .count();
    let tried = witnesses.len();
    (found >= MIN_SITEMAP_WITNESSES && found * 3 >= tried * 2).then(|| SitemapCheck {
        recipe: SitemapRecipe {
            url: map.to_owned(),
        },
        found,
        tried,
    })
}

/// A failure that should stop the probe; others just mean "not this way".
fn stops(e: &FetchError) -> bool {
    matches!(e, FetchError::Blocked(_))
}

/// The witnesses found so far and the links already opened.
#[derive(Default)]
struct WitnessHunt {
    found: Vec<Witness>,
    opened: Vec<Url>,
}

impl WitnessHunt {
    fn enough(&self) -> bool {
        self.found.len() >= WANTED_WITNESSES
    }

    /// Open `link` once and keep the book it shows, if any. `announced`: the
    /// link's path says it is a book; otherwise the page must not be a
    /// listing. `Ok(true)` when a new witness came out of it.
    async fn open(
        &mut self,
        fetcher: &dyn SiteFetcher,
        link: &Url,
        announced: bool,
    ) -> Result<bool, FetchError> {
        if self.opened.contains(link) {
            return Ok(false);
        }
        self.opened.push(link.clone());
        let page = match fetcher.get(link).await {
            Ok(p) => p,
            Err(e) if stops(&e) => return Err(e),
            Err(_) => return Ok(false),
        };
        let facts = if announced {
            book_facts(&page.html, &page.url)
        } else {
            single_book_facts(&page.html, &page.url)
        };
        let Some(f) = facts.filter(|f| !self.found.iter().any(|w| w.isbn == f.isbn)) else {
            return Ok(false);
        };
        self.found.push(Witness {
            isbn: f.isbn,
            title: f.title,
            authors: f.authors,
            url: Some(page.url.to_string()),
        });
        Ok(true)
    }

    /// Open `links`, whose path announces a book, until enough are found.
    async fn announced(
        &mut self,
        fetcher: &dyn SiteFetcher,
        links: Vec<Url>,
    ) -> Result<(), FetchError> {
        let links: Vec<Url> = links
            .into_iter()
            .filter(|l| !self.opened.contains(l))
            .take(MAX_WITNESS_LINKS)
            .collect();
        for link in links {
            if self.enough() {
                break;
            }
            self.open(fetcher, &link, true).await?;
        }
        Ok(())
    }

    /// `links` nothing announces, opened at a few places of their list;
    /// around one that shows a book, its neighbours of the same shape, where
    /// the other books are. At most [`MAX_SAMPLED_LINKS`] are opened.
    async fn sampled(
        &mut self,
        fetcher: &dyn SiteFetcher,
        links: &[Url],
    ) -> Result<(), FetchError> {
        let mut budget = MAX_SAMPLED_LINKS;
        let mut spots: Vec<usize> = (1..=SAMPLE_SPOTS)
            .map(|k| links.len() * k / (SAMPLE_SPOTS + 1))
            .collect();
        spots.dedup();
        for spot in spots {
            if self.enough() || budget == 0 || spot >= links.len() {
                break;
            }
            if self.opened.contains(&links[spot]) {
                continue;
            }
            budget -= 1;
            if !self.open(fetcher, &links[spot], false).await? {
                continue;
            }
            let shape = link_shape(&links[spot]);
            let neighbours = (spot + 1..links.len()).chain((0..spot).rev());
            for i in neighbours {
                if self.enough() || budget == 0 {
                    break;
                }
                if link_shape(&links[i]) != shape || self.opened.contains(&links[i]) {
                    continue;
                }
                budget -= 1;
                self.open(fetcher, &links[i], false).await?;
            }
        }
        Ok(())
    }

    /// Older books: among the `mapped` pages of the same kind as the
    /// witnesses found, those at one third and two thirds of the list, or the
    /// next one that shows a book. At most [`BACKLIST_WITNESSES`] are added,
    /// for [`MAX_BACKLIST_LINKS`] pages opened.
    async fn backlist(
        &mut self,
        fetcher: &dyn SiteFetcher,
        mapped: &[Url],
    ) -> Result<(), FetchError> {
        let kinds: Vec<(String, (usize, String))> = self
            .found
            .iter()
            .filter_map(|w| w.url.as_deref().and_then(|u| Url::parse(u).ok()))
            .map(|u| page_kind(&u))
            .collect();
        let pages: Vec<&Url> = mapped
            .iter()
            .filter(|u| kinds.contains(&page_kind(u)) && !self.opened.contains(u))
            .collect();
        let wanted = self.found.len() + BACKLIST_WITNESSES;
        let mut budget = MAX_BACKLIST_LINKS;
        for k in 1..=BACKLIST_WITNESSES {
            let start = pages.len() * k / (BACKLIST_WITNESSES + 1);
            for page in pages.iter().skip(start) {
                if self.found.len() >= wanted || budget == 0 {
                    return Ok(());
                }
                if self.opened.contains(page) {
                    continue;
                }
                budget -= 1;
                if self.open(fetcher, page, false).await? {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// What links to pages of one kind share: their depth and their extension.
fn link_shape(url: &Url) -> (usize, String) {
    let path = url.path().trim_end_matches('/');
    let last = path.rsplit('/').next().unwrap_or("");
    let extension = last.rsplit_once('.').map_or("", |(_, e)| e);
    (path.matches('/').count(), extension.to_ascii_lowercase())
}

/// A site map and the pages it lists.
struct SiteMap {
    url: Url,
    pages: Vec<Url>,
}

/// The site's maps: those robots.txt declares, else `/sitemap.xml`. In an
/// index, the maps named after books are read first (`product-sitemap.xml`);
/// [`MAX_SITEMAPS_READ`] files are read in all, and compressed maps are left
/// alone.
async fn site_maps(fetcher: &dyn SiteFetcher, home: &Page) -> Result<Vec<SiteMap>, FetchError> {
    let mut queue: Vec<Url> = fetcher.sitemaps(&home.url).await;
    if queue.is_empty() {
        queue.extend(home.url.join("/sitemap.xml"));
    }
    let mut seen: Vec<Url> = queue.clone();
    let mut maps: Vec<SiteMap> = Vec::new();
    let mut read = 0;
    while read < MAX_SITEMAPS_READ && !queue.is_empty() {
        let url = queue.remove(0);
        read += 1;
        let map = match fetcher.get(&url).await {
            Ok(p) => p,
            Err(e) if stops(&e) => return Err(e),
            Err(_) => continue,
        };
        let (is_index, locs) = sitemap_locs(&map.html, &home.url);
        if !is_index {
            if !locs.is_empty() {
                maps.push(SiteMap { url, pages: locs });
            }
            continue;
        }
        let named_after_books = |u: &Url| {
            let path = u.path().to_ascii_lowercase();
            BOOK_SITEMAP_WORDS.iter().any(|w| path.contains(w))
        };
        let (books, others): (Vec<Url>, Vec<Url>) = locs
            .into_iter()
            .filter(|u| !u.path().ends_with(".gz") && !seen.contains(u))
            .partition(named_after_books);
        let children: Vec<Url> = books.into_iter().chain(others).collect();
        seen.extend(children.iter().cloned());
        queue.splice(0..0, children);
    }
    Ok(maps)
}

/// An address without its query, fragment and final slash, to compare a
/// witness's page with a site map's entry.
fn page_key(url: &str) -> &str {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    url[..end].trim_end_matches('/')
}

/// The directory a page is in and its shape: pages of one kind share both.
fn page_kind(url: &Url) -> (String, (usize, String)) {
    let path = url.path().trim_end_matches('/');
    let parent = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    (parent.to_owned(), link_shape(url))
}

/// What the hunt for witnesses brought back.
pub struct Witnesses {
    pub found: Vec<Witness>,
    /// The site map that lists the witnesses' pages, hence the site's books.
    pub book_sitemap: Option<String>,
    /// The pages that map lists.
    pub book_pages: Vec<Url>,
}

/// Books the site sells, read on their own page: see [`hunt_witnesses`].
pub async fn find_witnesses(
    fetcher: &dyn SiteFetcher,
    home: &Page,
) -> Result<Vec<Witness>, FetchError> {
    Ok(hunt_witnesses(fetcher, home).await?.found)
}

/// Books the site sells, read on their own page. [`WANTED_WITNESSES`] recent
/// ones are looked for:
/// 1. behind the links of the home page whose path announces a book;
/// 2. the same way on the catalogue page the home page links, richer in
///    books;
/// 3. the same way among the pages the site's maps list;
/// 4. when no path announced a book, behind a few links of each of these
///    lists taken at different places: one that shows a single ISBN is a
///    book, and its neighbours are opened next.
///
/// Then up to [`BACKLIST_WITNESSES`] more are taken further down the site
/// map, among the pages of the same kind as those found: the home page shows
/// this month's books, and a recipe must hold for the older ones too.
///
/// A blocked site is an error; a page that fails is skipped.
pub async fn hunt_witnesses(
    fetcher: &dyn SiteFetcher,
    home: &Page,
) -> Result<Witnesses, FetchError> {
    let mut hunt = WitnessHunt::default();
    hunt.opened.push(home.url.clone());
    hunt.announced(fetcher, witness_links(&home.html, &home.url))
        .await?;

    let mut catalogue: Option<Page> = None;
    if !hunt.enough() {
        let link = catalogue_links(&home.html, &home.url)
            .into_iter()
            .find(|l| !hunt.opened.contains(l));
        if let Some(link) = link {
            hunt.opened.push(link.clone());
            match fetcher.get(&link).await {
                Ok(page) => {
                    hunt.announced(fetcher, witness_links(&page.html, &page.url))
                        .await?;
                    catalogue = Some(page);
                }
                Err(e) if stops(&e) => return Err(e),
                Err(_) => {}
            }
        }
    }

    let maps = site_maps(fetcher, home).await?;
    let mapped: Vec<Url> = maps.iter().flat_map(|m| m.pages.iter().cloned()).collect();
    if !hunt.enough() {
        hunt.announced(fetcher, announced_books(&mapped)).await?;
    }

    if hunt.found.is_empty() {
        let lists = catalogue
            .iter()
            .chain([home])
            .map(|page| other_links(&page.html, &page.url))
            .chain([unannounced_pages(&mapped)]);
        for links in lists {
            hunt.sampled(fetcher, &links).await?;
            if !hunt.found.is_empty() {
                break;
            }
        }
    }

    hunt.backlist(fetcher, &mapped).await?;

    let listed = |map: &SiteMap| {
        hunt.found
            .iter()
            .filter_map(|w| w.url.as_deref())
            .filter(|u| {
                map.pages
                    .iter()
                    .any(|p| page_key(p.as_str()) == page_key(u))
            })
            .count()
    };
    let book_sitemap = maps
        .iter()
        .map(|m| (listed(m), m))
        .filter(|(n, _)| *n > 0)
        // The first map wins a tie.
        .max_by_key(|(n, m)| {
            (
                *n,
                std::cmp::Reverse(maps.iter().position(|x| x.url == m.url)),
            )
        })
        .map(|(_, m)| m);
    Ok(Witnesses {
        book_sitemap: book_sitemap.map(|m| m.url.to_string()),
        book_pages: book_sitemap.map(|m| m.pages.clone()).unwrap_or_default(),
        found: hunt.found,
    })
}

/// A witness's address with its parts put back as placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WitnessPattern {
    template: String,
    /// The apostrophe rule the address shows; `None` when the title has no
    /// apostrophe (both rules give the same slug), so the witness has no say.
    apostrophe: Option<Apostrophe>,
}

/// Where `slug` is in `url` as whole words: not glued to a letter or a digit
/// on either side (`rose` is not the slug in `/primerose-des-bois`).
fn whole_slug_position(url: &str, slug: &str) -> Option<usize> {
    let glued = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    url.match_indices(slug).map(|(at, _)| at).find(|&at| {
        !glued(url[..at].chars().next_back()) && !glued(url[at + slug.len()..].chars().next())
    })
}

/// The witness's address with `{isbn}`, `{slug}` and `{author}` put back,
/// trying both apostrophe rules, every title order and author order.
///
/// An address carrying the ISBN but no slug of the title gives the address
/// with `{isbn}` alone (the page is reached by its ISBN only; whether that
/// holds for every book is for the other witnesses to tell). An address
/// without the ISBN is a pattern only when the title's slug is in it as whole
/// words: the page is then named by its title alone, and the ISBN it shows
/// tells whether it is the right one.
fn pattern_from_witness(w: &Witness) -> Option<WitnessPattern> {
    let raw = w.url.as_deref()?;
    let url = raw.split('?').next().unwrap_or(raw);
    let has_isbn = url.contains(&w.isbn);
    let with_isbn = url.replace(&w.isbn, "{isbn}");
    for rule in [Apostrophe::Drop, Apostrophe::Dash] {
        for title in title_variants(&w.title) {
            let slug = slugify(&title, rule);
            if slug.is_empty() {
                continue;
            }
            let authors = std::iter::once(String::new()).chain(author_variants(&w.authors));
            for author in authors {
                let full = if author.is_empty() {
                    slug.clone()
                } else {
                    format!("{slug}-{}", slugify(&author, rule))
                };
                let at = if has_isbn {
                    with_isbn.find(&full)
                } else {
                    whole_slug_position(&with_isbn, &full)
                };
                let Some(at) = at else {
                    continue;
                };
                let placeholder = if author.is_empty() {
                    "{slug}"
                } else {
                    "{slug}-{author}"
                };
                let other = match rule {
                    Apostrophe::Drop => Apostrophe::Dash,
                    Apostrophe::Dash => Apostrophe::Drop,
                };
                let telling = slugify(&title, other) != slug
                    || slugify(&author, other) != slugify(&author, rule);
                let mut template = with_isbn.clone();
                template.replace_range(at..at + full.len(), placeholder);
                return Some(WitnessPattern {
                    template,
                    apostrophe: telling.then_some(rule),
                });
            }
        }
    }
    has_isbn.then_some(WitnessPattern {
        template: with_isbn,
        apostrophe: None,
    })
}

/// The pattern most witnesses share (the first seen winning a tie), with
/// the apostrophe rule most of its telling witnesses show (`Drop` when none
/// tells).
fn vote(patterns: &[WitnessPattern]) -> Option<ProductRecipe> {
    let most_frequent = |items: Vec<&String>| -> Option<String> {
        items
            .iter()
            .max_by_key(|t| {
                let n = items.iter().filter(|u| u == t).count();
                (n, std::cmp::Reverse(items.iter().position(|u| u == *t)))
            })
            .map(|t| (*t).clone())
    };
    let template = most_frequent(patterns.iter().map(|p| &p.template).collect())?;
    let rules: Vec<Apostrophe> = patterns
        .iter()
        .filter(|p| p.template == template)
        .filter_map(|p| p.apostrophe)
        .collect();
    let dash = rules.iter().filter(|r| **r == Apostrophe::Dash).count();
    let apostrophe = if dash * 2 > rules.len() {
        Apostrophe::Dash
    } else {
        Apostrophe::Drop
    };
    Some(ProductRecipe {
        template,
        apostrophe,
        slug_ignored: false,
    })
}

/// The addresses a product recipe gives for a book, most likely first.
pub fn product_urls(recipe: &ProductRecipe, isbn: &str, title: &str, authors: &str) -> Vec<String> {
    let authors_tried = if recipe.template.contains("{author}") {
        author_variants(authors)
    } else {
        vec![String::new()]
    };
    let mut out: Vec<String> = Vec::new();
    for t in title_variants(title) {
        for a in &authors_tried {
            let u = recipe
                .template
                .replace("{isbn}", isbn)
                .replace("{slug}", &slugify(&t, recipe.apostrophe))
                .replace("{author}", &slugify(a, recipe.apostrophe));
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    if recipe.slug_ignored {
        out.truncate(1);
    }
    out
}

async fn probe_product(
    fetcher: &dyn SiteFetcher,
    witnesses: &[Witness],
) -> Result<Option<ProductCheck>, FetchError> {
    let patterns: Vec<WitnessPattern> = witnesses.iter().filter_map(pattern_from_witness).collect();
    let Some(mut recipe) = vote(&patterns) else {
        return Ok(None);
    };
    let mismatches = witnesses
        .iter()
        .filter(|w| {
            let url = w.url.as_deref().unwrap_or("");
            let url = url.split('?').next().unwrap_or(url);
            !product_urls(&recipe, &w.isbn, &w.title, &w.authors)
                .iter()
                .any(|u| u == url)
        })
        .count();

    // Does the server ignore the slug? Nothing to ask without one, nor
    // without the ISBN: the slug is then all the server has to go by.
    let w = &witnesses[0];
    if !recipe.template.contains("{slug}") || !recipe.template.contains("{isbn}") {
        return Ok(Some(ProductCheck {
            recipe,
            checked_on: witnesses.len() - mismatches,
            mismatches,
        }));
    }
    let wrong = recipe
        .template
        .replace("{isbn}", &w.isbn)
        .replace("{slug}", "x")
        .replace("{author}", "x");
    if let Ok(wrong) = Url::parse(&wrong) {
        match fetcher.get(&wrong).await {
            Ok(page) => {
                recipe.slug_ignored =
                    page_matches(&page.html, &w.isbn, "") == Some(PageMatch::Isbn);
                if recipe.slug_ignored {
                    recipe.template = bare_slug_segment(&recipe.template);
                }
            }
            Err(e) if stops(&e) => return Err(e),
            Err(_) => {}
        }
    }
    Ok(Some(ProductCheck {
        recipe,
        checked_on: witnesses.len() - mismatches,
        mismatches,
    }))
}

/// When the server ignores the slug, the path segment holding it is reduced
/// to `{slug}`: whatever the witness's address added there (an author's name
/// read nowhere else, a subtitle) would otherwise be frozen into every book's
/// address. A segment that also holds the ISBN is left as it is.
fn bare_slug_segment(template: &str) -> String {
    template
        .split('/')
        .map(|seg| {
            if seg.contains("{slug}") && !seg.contains("{isbn}") {
                "{slug}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A form as a search recipe. A POST form is submitted once with
/// `sample_isbn`: when it redirects to an address carrying it, that address
/// is the recipe (a GET); otherwise the POST itself is.
async fn form_recipe(
    fetcher: &dyn SiteFetcher,
    form: &super::page::SearchForm,
    sample_isbn: &str,
) -> Result<Option<SearchRecipe>, FetchError> {
    if !form.post {
        let query = form_urlencode(&form.fields);
        let mut url = form.action.to_string();
        url.push('?');
        url.push_str(&query);
        return Ok(Some(SearchRecipe {
            url,
            post_fields: None,
            by_isbn: false,
        }));
    }
    let filled: Vec<(String, String)> = form
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("{q}", sample_isbn)))
        .collect();
    match fetcher.post_form(&form.action, &filled).await {
        Ok(PostAnswer::Redirect(to)) if to.as_str().contains(sample_isbn) => {
            Ok(Some(SearchRecipe {
                url: to.as_str().replace(sample_isbn, "{q}"),
                post_fields: None,
                by_isbn: false,
            }))
        }
        Ok(_) => Ok(Some(SearchRecipe {
            url: form.action.to_string(),
            post_fields: Some(form.fields.clone()),
            by_isbn: false,
        })),
        Err(e) if stops(&e) => Err(e),
        // A form robots.txt forbids, or one that fails: not a recipe.
        Err(_) => Ok(None),
    }
}

/// Form-encode fields, leaving the `{q}` placeholder readable.
fn form_urlencode(fields: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in fields {
        ser.append_pair(k, v);
    }
    ser.finish().replace("%7Bq%7D", "{q}")
}

/// Run a search. Returns the answer and the address asked, against which a
/// redirect is judged.
pub async fn run_search(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
    query: &str,
) -> Result<(Page, Url), FetchError> {
    match &recipe.post_fields {
        None => {
            let encoded = urlencoding::encode(query);
            let url = Url::parse(&recipe.url.replace("{q}", &encoded))
                .map_err(|_| FetchError::Refused)?;
            let page = fetcher.get(&url).await?;
            Ok((page, url))
        }
        Some(fields) => {
            let action = Url::parse(&recipe.url).map_err(|_| FetchError::Refused)?;
            let filled: Vec<(String, String)> = fields
                .iter()
                .map(|(k, v)| (k.clone(), v.replace("{q}", query)))
                .collect();
            match fetcher.post_form(&action, &filled).await? {
                PostAnswer::Page(page) => Ok((page, action)),
                PostAnswer::Redirect(to) => {
                    let page = fetcher.get(&to).await?;
                    Ok((page, action))
                }
            }
        }
    }
}

/// The server sent us to the book itself: the final path is not the one
/// asked and carries the ISBN (a `/search/<isbn>` result page does not count).
pub fn redirected_to_book(page: &Page, asked: &Url, isbn: &str) -> bool {
    let fin = page.url.path().trim_end_matches('/');
    fin != asked.path().trim_end_matches('/') && fin.contains(isbn)
}

/// Whether a search for `query` finds `w`, and the size of the answer (an
/// answer whose size never changes is drawn by JavaScript).
async fn search_finds(
    fetcher: &dyn SiteFetcher,
    recipe: &SearchRecipe,
    query: &str,
    w: &Witness,
) -> Result<(bool, Option<usize>), FetchError> {
    match run_search(fetcher, recipe, query).await {
        Ok((page, asked)) => {
            let found = redirected_to_book(&page, &asked, &w.isbn)
                || !result_links(&page.html, &page.url, &w.isbn, &w.title, &w.authors).is_empty();
            Ok((found, Some(page.html.len())))
        }
        Err(e) if stops(&e) => Err(e),
        Err(_) => Ok((false, None)),
    }
}

async fn validate_search(
    fetcher: &dyn SiteFetcher,
    recipe: SearchRecipe,
    witnesses: &[Witness],
) -> Result<SearchCheck, FetchError> {
    let mut check = SearchCheck {
        recipe,
        found_by_isbn: 0,
        found_by_title: 0,
        js_results: false,
        broken: false,
        note: None,
    };
    let control = match run_search(fetcher, &check.recipe, CONTROL_QUERY).await {
        Ok((page, _)) => page,
        Err(e) if stops(&e) => return Err(e),
        Err(e) => {
            check.note = Some(format!("control query: {}", describe(&e)));
            return Ok(check);
        }
    };
    if control.html.len() < MIN_RESULT_PAGE_BYTES
        || !control.html.to_ascii_lowercase().contains("<a ")
    {
        check.broken = true;
        check.note = Some(control.html.chars().take(120).collect());
        return Ok(check);
    }
    let mut sizes = vec![control.html.len()];
    let testable: Vec<&Witness> = witnesses
        .iter()
        .filter(|w| {
            result_links(&control.html, &control.url, &w.isbn, &w.title, &w.authors).is_empty()
        })
        .collect();
    if testable.is_empty() {
        check.note = Some("the control page already links the witnesses".into());
        return Ok(check);
    }
    for (tried, w) in testable.iter().enumerate() {
        let (found, size) = search_finds(fetcher, &check.recipe, &w.isbn, w).await?;
        sizes.extend(size);
        if found {
            check.found_by_isbn += 1;
        } else {
            let (found, size) = search_finds(fetcher, &check.recipe, &w.title, w).await?;
            sizes.extend(size);
            check.found_by_title += usize::from(found);
        }
        // Two witnesses missed: drop this recipe.
        if check.found_by_isbn + check.found_by_title == 0 && tried + 1 >= 2 {
            break;
        }
    }
    sizes.sort_unstable();
    sizes.dedup();
    check.js_results = check.found_by_isbn + check.found_by_title == 0 && sizes.len() == 1;
    check.recipe.by_isbn = check.found_by_isbn > 0;
    Ok(check)
}

/// The best search recipe of the site, if one finds witnesses, and every
/// recipe tried.
async fn probe_search(
    fetcher: &dyn SiteFetcher,
    home: &Page,
    witnesses: &[Witness],
) -> Result<(Option<SearchCheck>, Vec<SearchCheck>), FetchError> {
    let mut pages = vec![home.clone()];
    for path in SEARCH_PAGES {
        let Ok(url) = home.url.join(path) else {
            continue;
        };
        match fetcher.get(&url).await {
            Ok(p) => pages.push(p),
            Err(e) if stops(&e) => return Err(e),
            Err(_) => {}
        }
    }
    // The same form on every page (a header search box) is sent once: a
    // POST form is submitted to turn it into a recipe.
    let mut forms: Vec<super::page::SearchForm> = Vec::new();
    for page in &pages {
        for form in search_forms(&page.html, &page.url)
            .into_iter()
            .take(MAX_FORMS_PER_PAGE)
        {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    let mut recipes: Vec<SearchRecipe> = Vec::new();
    for form in forms {
        if let Some(r) = form_recipe(fetcher, &form, &witnesses[0].isbn).await?
            && !recipes
                .iter()
                .any(|x| x.url == r.url && x.post_fields == r.post_fields)
        {
            recipes.push(r);
        }
    }
    let origin = home.url.origin().ascii_serialization();
    for guess in GUESSES {
        let url = format!("{origin}{guess}");
        if !recipes.iter().any(|r| r.url == url) {
            recipes.push(SearchRecipe {
                url,
                post_fields: None,
                by_isbn: false,
            });
        }
    }

    let mut tried = Vec::new();
    let mut best: Option<(usize, SearchCheck)> = None;
    let finds = |check: &SearchCheck| check.found_by_isbn + check.found_by_title > 0;
    let (wordpress, alias) = (
        format!("{origin}{WORDPRESS_SEARCH}"),
        format!("{origin}{WORDPRESS_SEARCH_ALIAS}"),
    );
    for recipe in recipes {
        let same_search_found = recipe.url == alias
            && tried
                .iter()
                .any(|t: &SearchCheck| t.recipe.url == wordpress && finds(t));
        if same_search_found {
            continue;
        }
        let check = validate_search(fetcher, recipe, witnesses).await?;
        let score = check.found_by_isbn * 2 + check.found_by_title;
        let all_by_isbn = check.found_by_isbn == witnesses.len();
        if score > 0 && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, check.clone()));
        }
        tried.push(check);
        if all_by_isbn {
            break;
        }
    }
    Ok((best.map(|(_, c)| c), tried))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::test_site::{FakeSite, book_page, padding};

    const HOME: &str = "https://editions.example/";

    fn site_with_books() -> FakeSite {
        let books = [
            ("9782000000011", "Le Grand Voyage", "le-grand-voyage"),
            ("9782000000028", "L'Hiver des loups", "lhiver-des-loups"),
            ("9782000000035", "Martin Eden", "martin-eden"),
        ];
        let home: String = books
            .iter()
            .map(|(isbn, _, slug)| format!(r#"<a href="/livre/{slug}-{isbn}">x</a>"#))
            .collect();
        let mut site = FakeSite::default().page(HOME, &format!("<main>{home}</main>"));
        for (isbn, title, slug) in books {
            site = site.page(
                &format!("https://editions.example/livre/{slug}-{isbn}"),
                &book_page(isbn, title),
            );
        }
        site
    }

    #[tokio::test]
    async fn finds_and_checks_a_product_pattern() {
        let site = site_with_books();
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Ok);
        assert_eq!(probe.witnesses.len(), 3);
        let product = probe.product.unwrap();
        assert_eq!(
            product.recipe.template,
            "https://editions.example/livre/{slug}-{isbn}"
        );
        assert_eq!(product.recipe.apostrophe, Apostrophe::Drop);
        assert!(
            !product.recipe.slug_ignored,
            "the fake site 404s a wrong slug"
        );
        assert_eq!(product.checked_on, 3);
        assert!(probe.search.is_none());
        assert_eq!(probe.tried.len(), GUESSES.len());
    }

    fn w(isbn: &str, title: &str, url: &str) -> Witness {
        Witness {
            isbn: isbn.into(),
            title: title.into(),
            authors: String::new(),
            url: Some(url.into()),
        }
    }

    #[test]
    fn only_titles_with_an_apostrophe_vote_on_its_rule() {
        let patterns: Vec<WitnessPattern> = [
            w(
                "9782000000011",
                "Le Grand Voyage",
                "https://e.example/le-grand-voyage-9782000000011",
            ),
            w(
                "9782000000028",
                "L'Hiver des loups",
                "https://e.example/l-hiver-des-loups-9782000000028",
            ),
            w(
                "9782000000035",
                "Martin Eden",
                "https://e.example/martin-eden-9782000000035",
            ),
        ]
        .iter()
        .filter_map(pattern_from_witness)
        .collect();
        let recipe = vote(&patterns).unwrap();
        assert_eq!(recipe.template, "https://e.example/{slug}-{isbn}");
        assert_eq!(recipe.apostrophe, Apostrophe::Dash);
    }

    #[test]
    fn a_page_named_by_its_isbn_alone_is_a_pattern() {
        let patterns: Vec<WitnessPattern> = [
            w(
                "9782000000011",
                "Le Grand Voyage",
                "https://e.example/livre/9782000000011",
            ),
            w(
                "9782000000028",
                "Martin Eden",
                "https://e.example/livre/9782000000028",
            ),
        ]
        .iter()
        .filter_map(pattern_from_witness)
        .collect();
        let recipe = vote(&patterns).unwrap();
        assert_eq!(recipe.template, "https://e.example/livre/{isbn}");
        assert_eq!(
            product_urls(&recipe, "9782000000035", "Autre titre", ""),
            ["https://e.example/livre/9782000000035"]
        );
    }

    #[tokio::test]
    async fn a_search_page_that_ignores_the_query_is_not_a_recipe() {
        // `/?s=` answers the home page, which links every witness.
        let mut site = site_with_books();
        let home = site.pages[HOME].clone() + &padding();
        site.pages.insert(HOME.into(), home.clone());
        site.pages.insert(format!("{HOME}?s={CONTROL_QUERY}"), home);
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        let tried = probe
            .tried
            .iter()
            .find(|t| t.recipe.url.ends_with("/?s={q}"))
            .unwrap();
        assert_eq!(tried.found_by_isbn + tried.found_by_title, 0);
        assert!(tried.note.as_deref().unwrap().contains("already links"));
    }

    #[tokio::test]
    async fn finds_a_search_form_and_validates_it_by_isbn() {
        let mut site = FakeSite::default().page(
            HOME,
            &format!(
                r#"<form action="/recherche"><input type="search" name="texte"></form>{}"#,
                padding()
            ),
        );
        let isbn = "9782000000011";
        let w = Witness {
            isbn: isbn.into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        site = site
            .page(
                &format!("{HOME}recherche?texte={CONTROL_QUERY}"),
                &padding(),
            )
            .page(
                &format!("{HOME}recherche?texte={isbn}"),
                &format!(
                    r#"{}<a href="/ouvrage/{isbn}">Le Grand Voyage</a>"#,
                    padding()
                ),
            );
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Ok);
        let search = probe.search.unwrap();
        assert_eq!(
            search.recipe.url,
            "https://editions.example/recherche?texte={q}"
        );
        assert!(search.recipe.by_isbn);
        assert_eq!(search.found_by_isbn, 1);
        // Found on the first recipe: the guesses are not tried.
        assert_eq!(probe.tried.len(), 1);
    }

    #[tokio::test]
    async fn the_alias_of_a_search_that_works_is_not_tried() {
        let title = "Le Grand Voyage";
        let w = Witness {
            isbn: "9782000000011".into(),
            title: title.into(),
            authors: String::new(),
            url: None,
        };
        // WordPress finds by title only, so the other guesses are still tried.
        let site = FakeSite::default()
            .page(HOME, &padding())
            .page(&format!("{HOME}?s={CONTROL_QUERY}"), &padding())
            .page(&format!("{HOME}?s=9782000000011"), &padding())
            .page(
                &format!("{HOME}?s=Le%20Grand%20Voyage"),
                &format!(
                    r#"{}<a href="/livre/le-grand-voyage">{title}</a>"#,
                    padding()
                ),
            );
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        assert_eq!(probe.search.unwrap().found_by_title, 1);
        assert_eq!(probe.tried.len(), GUESSES.len() - 1);
        assert!(
            !probe
                .tried
                .iter()
                .any(|t| t.recipe.url.ends_with("/search/{q}"))
        );
    }

    #[tokio::test]
    async fn the_same_answer_to_every_query_means_javascript_results() {
        let shell = padding();
        let mut site = FakeSite::default().page(HOME, &shell);
        let w = Witness {
            isbn: "9782000000011".into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        for q in [CONTROL_QUERY, "9782000000011", "Le%20Grand%20Voyage"] {
            for guess in GUESSES {
                site = site.page(
                    &format!("https://editions.example{}", guess.replace("{q}", q)),
                    &shell,
                );
            }
        }
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::JsResults);
    }

    #[tokio::test]
    async fn tries_a_bounded_number_of_forms_per_page() {
        let forms: String = (0..12)
            .map(|i| format!(r#"<form action="/f{i}"><input type="search" name="q"></form>"#))
            .collect();
        let site = FakeSite::default().page(HOME, &forms);
        let w = Witness {
            isbn: "9782000000011".into(),
            title: "Le Grand Voyage".into(),
            authors: String::new(),
            url: None,
        };
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[w], "2026-11-13").await;
        let from_forms = probe
            .tried
            .iter()
            .filter(|t| t.recipe.url.contains("/f"))
            .count();
        assert_eq!(from_forms, MAX_FORMS_PER_PAGE);
    }

    #[tokio::test]
    async fn a_blocked_site_stops_the_probe() {
        let site = FakeSite {
            blocked: true,
            ..FakeSite::default()
        };
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Blocked);
        assert_eq!(site.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_home_page_without_books_has_no_witness() {
        let site = FakeSite::default().page(HOME, "<a href='/contact'>Contact</a>");
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::NoWitness);
    }

    /// A book page that only prints its ISBN, as small sites do.
    fn plain_book_page(isbn: &str, title: &str) -> String {
        format!("<h1>{title}</h1><p>ISBN {isbn}</p>")
    }

    #[tokio::test]
    async fn follows_the_catalogue_when_the_home_page_shows_no_book() {
        let site = FakeSite::default()
            .page(
                HOME,
                r#"<nav><a href="/catalogue">Catalogue</a></nav><a href="/contact">Contact</a>"#,
            )
            .page(
                "https://editions.example/catalogue",
                r#"<a href="/livre/le-grand-voyage">x</a><a href="/livre/martin-eden">y</a>"#,
            )
            .page(
                "https://editions.example/livre/le-grand-voyage",
                &book_page("9782000000011", "Le Grand Voyage"),
            )
            .page(
                "https://editions.example/livre/martin-eden",
                &book_page("9782000000035", "Martin Eden"),
            );
        let home = site.get(&Url::parse(HOME).unwrap()).await.unwrap();
        let witnesses = find_witnesses(&site, &home).await.unwrap();
        let isbns: Vec<&str> = witnesses.iter().map(|w| w.isbn.as_str()).collect();
        assert_eq!(isbns, ["9782000000011", "9782000000035"]);
    }

    #[tokio::test]
    async fn samples_unnamed_links_and_reads_the_neighbours_of_a_book() {
        // Twelve links nothing tells apart: pages 4 to 9 are books.
        let links: String = (0..12)
            .map(|i| format!(r#"<a href="/Titre{i}">t</a>"#))
            .collect();
        let mut site = FakeSite::default().page(HOME, &links);
        for i in 0..12 {
            let html = if (4..10).contains(&i) {
                plain_book_page(&format!("97820000000{i:02}"), &format!("Titre{i}"))
            } else {
                "<h1>Rubrique</h1>".to_owned()
            };
            site = site.page(&format!("https://editions.example/Titre{i}"), &html);
        }
        let home = site.get(&Url::parse(HOME).unwrap()).await.unwrap();
        let witnesses = find_witnesses(&site, &home).await.unwrap();
        assert_eq!(witnesses.len(), WANTED_WITNESSES);
        let asked = site.asked.lock().unwrap().len();
        // The home page, the missing site map, then the sampled links.
        assert!(
            asked <= 2 + MAX_SAMPLED_LINKS,
            "{asked} requests for a sampled page"
        );
    }

    #[tokio::test]
    async fn reads_the_books_of_the_site_map_robots_txt_declares() {
        // The home page links no book.
        let mut site = FakeSite {
            sitemaps: vec!["https://editions.example/plan.xml".into()],
            ..FakeSite::default()
        }
        .page(HOME, r#"<a href="/contact">Contact</a>"#)
        .page(
            "https://editions.example/plan.xml",
            "<sitemapindex><sitemap><loc>https://editions.example/page-sitemap.xml</loc></sitemap>\
             <sitemap><loc>https://editions.example/product-sitemap.xml</loc></sitemap></sitemapindex>",
        )
        .page(
            "https://editions.example/product-sitemap.xml",
            "<urlset><url><loc>https://editions.example/livre/le-grand-voyage</loc></url>\
             <url><loc>https://editions.example/livre/martin-eden</loc></url></urlset>",
        );
        for (isbn, title, slug) in [
            ("9782000000011", "Le Grand Voyage", "le-grand-voyage"),
            ("9782000000035", "Martin Eden", "martin-eden"),
        ] {
            site = site.page(
                &format!("https://editions.example/livre/{slug}"),
                &book_page(isbn, title),
            );
        }
        let home = site.get(&Url::parse(HOME).unwrap()).await.unwrap();
        let witnesses = find_witnesses(&site, &home).await.unwrap();
        assert_eq!(witnesses.len(), 2);
        // The map named after books is read before the other.
        let asked = site.asked.lock().unwrap();
        let product = asked.iter().position(|u| u.contains("product-sitemap"));
        let page = asked.iter().position(|u| u.contains("page-sitemap"));
        assert!(product.is_some() && page.is_none_or(|p| product.unwrap() < p));
    }

    #[tokio::test]
    async fn a_sampled_listing_is_not_a_witness() {
        let listing: String = (0..8)
            .map(|i| format!("<p>ISBN 97820000000{i:02}</p>"))
            .collect();
        let site = FakeSite::default()
            .page(HOME, r#"<a href="/Essais">Essais</a>"#)
            .page(
                "https://editions.example/Essais",
                &format!("<h1>Essais</h1>{listing}"),
            );
        let home = site.get(&Url::parse(HOME).unwrap()).await.unwrap();
        assert!(find_witnesses(&site, &home).await.unwrap().is_empty());
    }

    /// A site whose book pages are named by the title alone.
    fn site_named_by_titles(titles: &[(&str, &str, &str)]) -> FakeSite {
        let mut site = FakeSite::default();
        for (isbn, title, slug) in titles {
            site = site.page(
                &format!("https://editions.example/livre/{slug}/"),
                &book_page(isbn, title),
            );
        }
        site
    }

    #[tokio::test]
    async fn pages_named_by_the_title_alone_are_a_pattern() {
        let books = [
            ("9782000000011", "Le Grand Voyage", "le-grand-voyage"),
            ("9782000000028", "L'Hiver des loups", "l-hiver-des-loups"),
            ("9782000000035", "Martin Eden", "martin-eden"),
        ];
        let home: String = books
            .iter()
            .map(|(_, _, slug)| format!(r#"<a href="/livre/{slug}/">x</a>"#))
            .collect();
        let site = site_named_by_titles(&books).page(HOME, &home);
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert_eq!(probe.status, ProbeStatus::Ok);
        let product = probe.product.unwrap();
        assert_eq!(
            product.recipe.template,
            "https://editions.example/livre/{slug}/"
        );
        assert_eq!(product.recipe.apostrophe, Apostrophe::Dash);
        assert_eq!(product.checked_on, 3);
        assert!(!product.recipe.slug_ignored);
        // No "does the server ignore the slug" request: there is no ISBN to
        // find the book by.
        let asked = site.asked.lock().unwrap();
        assert!(!asked.iter().any(|u| u.ends_with("/livre/x/")));
    }

    #[test]
    fn a_title_inside_another_word_is_not_the_slug() {
        let glued = w(
            "9782000000011",
            "Rose",
            "https://e.example/livre/primerose-des-bois",
        );
        assert_eq!(pattern_from_witness(&glued), None);
        let whole = w("9782000000011", "Rose", "https://e.example/livre/rose.html");
        assert_eq!(
            pattern_from_witness(&whole).unwrap().template,
            "https://e.example/livre/{slug}.html"
        );
    }

    #[tokio::test]
    async fn a_single_page_named_by_its_title_is_not_a_pattern() {
        let books = [("9782000000011", "Le Grand Voyage", "le-grand-voyage")];
        let site =
            site_named_by_titles(&books).page(HOME, r#"<a href="/livre/le-grand-voyage/">x</a>"#);
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-13").await;
        assert!(probe.product.is_none());
    }

    #[tokio::test]
    async fn takes_older_books_from_the_site_map() {
        let recent = [
            ("9782000000011", "Le Grand Voyage", "le-grand-voyage"),
            ("9782000000028", "Martin Eden", "martin-eden"),
            ("9782000000035", "Croc-Blanc", "croc-blanc"),
        ];
        let home: String = recent
            .iter()
            .map(|(_, _, slug)| format!(r#"<a href="/livre/{slug}/">x</a>"#))
            .collect();
        let mut site = site_named_by_titles(&recent).page(HOME, &home);
        // The map lists the three recent books, nine older ones and a page
        // of another kind.
        let mut locs: Vec<String> = recent
            .iter()
            .map(|(_, _, slug)| format!("https://editions.example/livre/{slug}/"))
            .collect();
        for i in 0..9 {
            let url = format!("https://editions.example/livre/fonds-{i}/");
            site = site.page(
                &url,
                &book_page(&format!("97820000001{i:02}"), &format!("Fonds {i}")),
            );
            locs.push(url);
        }
        locs.push("https://editions.example/contact".into());
        let xml: String = locs
            .iter()
            .map(|l| format!("<url><loc>{l}</loc></url>"))
            .collect();
        site = site.page(
            "https://editions.example/sitemap.xml",
            &format!("<urlset>{xml}</urlset>"),
        );

        let home = site.get(&Url::parse(HOME).unwrap()).await.unwrap();
        let hunted = hunt_witnesses(&site, &home).await.unwrap();
        let titles: Vec<&str> = hunted.found.iter().map(|w| w.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Le Grand Voyage",
                "Martin Eden",
                "Croc-Blanc",
                "Fonds 3",
                "Fonds 6"
            ]
        );
        assert_eq!(
            hunted.book_sitemap.as_deref(),
            Some("https://editions.example/sitemap.xml")
        );
    }

    #[tokio::test]
    async fn a_site_map_is_the_recipe_of_books_filed_under_sections() {
        // Each book sits under its own section: no pattern rebuilds two of
        // them, and the site has no search.
        let books = [
            ("9782000000011", "Le Grand Voyage", "recits/le-grand-voyage"),
            ("9782000000028", "Martin Eden", "romans/martin-eden"),
            ("9782000000035", "Croc-Blanc", "jeunesse/croc-blanc"),
        ];
        let mut site = FakeSite::default();
        let mut home = String::new();
        let mut xml = String::new();
        for (isbn, title, path) in books {
            let url = format!("https://editions.example/livres/{path}/");
            site = site.page(&url, &book_page(isbn, title));
            home.push_str(&format!(r#"<a href="/livres/{path}/">x</a>"#));
            xml.push_str(&format!("<url><loc>{url}</loc></url>"));
        }
        let map = "https://editions.example/sitemap.xml";
        let site = site
            .page(HOME, &home)
            .page(map, &format!("<urlset>{xml}</urlset>"));
        let probe = probe_site(&site, &Url::parse(HOME).unwrap(), &[], "2026-11-14").await;
        assert!(probe.product.is_none() && probe.search.is_none());
        let sitemap = probe.sitemap.unwrap();
        assert_eq!(sitemap.recipe.url, map);
        assert_eq!((sitemap.found, sitemap.tried), (3, 3));
        assert_eq!(probe.status, ProbeStatus::Ok);
    }

    #[test]
    fn product_urls_try_title_orders_and_authors() {
        let recipe = ProductRecipe {
            template: "https://e.example/{slug}-{author}-{isbn}".into(),
            apostrophe: Apostrophe::Dash,
            slug_ignored: false,
        };
        let urls = product_urls(
            &recipe,
            "9782000000011",
            "Saga, tome 1 : L'Aube",
            "Anne Auteur",
        );
        assert_eq!(
            urls[0],
            "https://e.example/saga-tome-1-l-aube-anne-auteur-9782000000011"
        );
        assert!(urls.contains(
            &"https://e.example/l-aube-saga-tome-1-auteur-anne-9782000000011".to_string()
        ));

        let ignored = ProductRecipe {
            slug_ignored: true,
            ..recipe
        };
        assert_eq!(
            product_urls(
                &ignored,
                "9782000000011",
                "Saga, tome 1 : L'Aube",
                "Anne Auteur"
            )
            .len(),
            1
        );
    }

    #[test]
    fn an_ignored_slug_keeps_no_frozen_words() {
        assert_eq!(
            bare_slug_segment(
                "https://www.seuil.com/ouvrage/{slug}-contre-enquete-jennifer-tamas/{isbn}"
            ),
            "https://www.seuil.com/ouvrage/{slug}/{isbn}"
        );
        assert_eq!(
            bare_slug_segment("https://e.example/livre/{slug}-{isbn}/"),
            "https://e.example/livre/{slug}-{isbn}/"
        );
    }

    #[test]
    fn a_search_result_path_with_the_isbn_is_not_a_redirect_to_the_book() {
        let asked = Url::parse("https://e.example/search/9782000000011").unwrap();
        let same = Page {
            url: asked.clone(),
            html: String::new(),
        };
        assert!(!redirected_to_book(&same, &asked, "9782000000011"));
        let book = Page {
            url: Url::parse("https://e.example/livre/x-9782000000011").unwrap(),
            html: String::new(),
        };
        assert!(redirected_to_book(&book, &asked, "9782000000011"));
    }
}
