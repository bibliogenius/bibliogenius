//! Reading a publisher's pages: what book a page shows, which links of a
//! search result page lead to a given book, which links of a home page are
//! books, and which forms are search forms. Pure functions over HTML, shared
//! by the maintenance probe and the app's lookup.

use std::collections::HashSet;
use std::sync::LazyLock;

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;

use super::text::{find_isbns, fold, strip_isbn_separators, title_words};
use crate::utils::isbn::to_isbn10;

/// Deepest JSON-LD nesting walked; deeper values are ignored.
const MAX_JSON_LD_DEPTH: usize = 8;

/// Words found in the path of book pages on sites whose book URLs carry no
/// ISBN, matched inside a lowercase path segment (`livre` also reads
/// `/livres/` and `/fiche-livre-x/`). `accueil` is the default category of
/// PrestaShop shops.
const PRODUCT_HINTS: [&str; 11] = [
    "livre",
    "ouvrage",
    "catalogue",
    "product",
    "produit",
    "book",
    "titre",
    "publication",
    "accueil",
    "roman",
    "essai",
];

/// Words of a path segment naming a book by itself (`fiche-livre-serres-rose`)
/// rather than a section (`nouveaux-livres`).
const MIN_NAMED_SEGMENT_WORDS: usize = 3;

/// Words that name the page listing a publisher's books, most telling first,
/// compared with the folded words of a link's last path segment and text.
const CATALOGUE_WORDS: [&str; 10] = [
    "catalogue",
    "catalog",
    "livres",
    "ouvrages",
    "publications",
    "parutions",
    "nouveautes",
    "boutique",
    "shop",
    "collections",
];

/// Extensions of links that are files, not pages.
const FILE_EXTENSIONS: [&str; 9] = [
    ".pdf", ".jpg", ".jpeg", ".png", ".gif", ".webp", ".zip", ".epub", ".xml",
];

/// A page printing more ISBNs than this in its text is a list of books (a
/// book page may print its other formats and a few related titles).
const MAX_BOOK_PAGE_ISBNS: usize = 4;

/// Path fragments of pages about many books (an author, a collection): a
/// result link to one of them is worth less than one to a book.
const LISTING_HINTS: [&str; 5] = ["/auteur", "/author", "/collection", "/categor", "/serie"];

/// Input names that search forms use.
const SEARCH_NAMES: [&str; 15] = [
    "q",
    "s",
    "search",
    "query",
    "keywords",
    "keyword",
    "recherche",
    "s[q]",
    "global_s",
    "search_query",
    "text",
    "term",
    "k",
    "motcle",
    "searchtext",
];

static JSON_LD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("script[type]").unwrap());
static H1: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").unwrap());
static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static HEADS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1, title").unwrap());
static OG_TITLE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"meta[property="og:title"]"#).unwrap());
static LINKS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static FORMS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("form").unwrap());
static META_CONTENT: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("meta[content]").unwrap());
static INPUTS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("input[name]").unwrap());

/// What a book page says about its book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFacts {
    pub isbn: String,
    pub title: String,
    /// Empty when the page does not name an author in a form we read.
    pub authors: String,
}

/// How a page was recognised as the wanted book's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageMatch {
    /// The page shows the book's ISBN.
    Isbn,
    /// Only the title's words were found in the page's headings.
    Title,
}

/// A search form found on a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchForm {
    pub post: bool,
    /// For a GET form, without its query (merged into `fields`); a POST
    /// form's action keeps its query, which the server reads there.
    pub action: Url,
    /// Every field sent, the query field's value being `{q}`.
    pub fields: Vec<(String, String)>,
}

/// The host of `url` without a leading `www.`, lowercase.
pub fn bare_host(url: &Url) -> String {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    host.strip_prefix("www.").map(str::to_owned).unwrap_or(host)
}

/// Whether `url` is on the site whose bare host is `host` (subdomains included).
pub fn same_site(url: &Url, host: &str) -> bool {
    let h = bare_host(url);
    h == host || h.ends_with(&format!(".{host}"))
}

fn element_text(e: &ElementRef) -> String {
    e.text().collect::<Vec<_>>().join(" ")
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decoded_path(url: &Url) -> String {
    urlencoding::decode(url.path())
        .map(|p| p.into_owned())
        .unwrap_or_else(|_| url.path().to_owned())
}

/// Every JSON object of the page's JSON-LD, nested ones included.
fn json_ld_objects(doc: &Html) -> Vec<serde_json::Map<String, Value>> {
    fn walk(v: &Value, depth: usize, out: &mut Vec<serde_json::Map<String, Value>>) {
        if depth > MAX_JSON_LD_DEPTH {
            return;
        }
        match v {
            Value::Array(items) => items.iter().for_each(|i| walk(i, depth + 1, out)),
            Value::Object(map) => {
                out.push(map.clone());
                map.values().for_each(|c| walk(c, depth + 1, out));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let scripts = doc.select(&JSON_LD).filter(|s| {
        s.value()
            .attr("type")
            .is_some_and(|t| t.to_ascii_lowercase().contains("ld+json"))
    });
    for script in scripts {
        if let Ok(v) = serde_json::from_str::<Value>(&script.text().collect::<String>()) {
            walk(&v, 0, &mut out);
        }
    }
    out
}

fn has_type(map: &serde_json::Map<String, Value>, names: &[&str]) -> bool {
    let is = |s: &str| names.contains(&s.rsplit(['/', ':']).next().unwrap_or(s));
    match map.get("@type") {
        Some(Value::String(s)) => is(s),
        Some(Value::Array(items)) => items.iter().any(|i| i.as_str().is_some_and(is)),
        _ => false,
    }
}

/// The ISBN, title and author of a book page, or `None` when the page does
/// not look like one. The ISBN comes from the JSON-LD `Book` or `Product`,
/// else from the URL, else is the one the page prints most often.
pub fn book_facts(html: &str, url: &Url) -> Option<BookFacts> {
    read_book_facts(html, url, usize::MAX)
}

/// [`book_facts`] for a page nothing announces as a book's (a link picked at
/// random): when the ISBN is only read in the text, a page printing more than
/// [`MAX_BOOK_PAGE_ISBNS`] different ones is a listing, not a book.
pub fn single_book_facts(html: &str, url: &Url) -> Option<BookFacts> {
    read_book_facts(html, url, MAX_BOOK_PAGE_ISBNS)
}

fn read_book_facts(html: &str, url: &Url, max_text_isbns: usize) -> Option<BookFacts> {
    let doc = Html::parse_document(html);
    let (mut isbn, mut title, mut author) = (None::<String>, None::<String>, None::<String>);
    for node in json_ld_objects(&doc) {
        if !has_type(&node, &["Book", "Product"]) {
            continue;
        }
        for key in ["isbn", "gtin13", "gtin"] {
            let raw = match node.get(key) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => continue,
            };
            let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
            if digits.len() == 13 && find_isbns(&digits).len() == 1 && isbn.is_none() {
                isbn = Some(digits);
            }
        }
        if title.is_none() {
            title = node.get("name").and_then(Value::as_str).map(str::to_owned);
        }
        let a = match node.get("author") {
            Some(Value::Array(items)) => items.first(),
            other => other,
        };
        if author.is_none() {
            author = match a {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Object(m)) => m.get("name").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            };
        }
    }
    if isbn.is_none() {
        isbn = find_isbns(url.path()).into_iter().next();
    }
    if isbn.is_none() {
        let text: String = doc.root_element().text().collect::<Vec<_>>().join(" ");
        let printed = find_isbns(&strip_isbn_separators(&text));
        let mut distinct: Vec<&String> = Vec::new();
        for i in &printed {
            if !distinct.contains(&i) {
                distinct.push(i);
            }
        }
        if distinct.len() > max_text_isbns {
            return None;
        }
        isbn = most_frequent(printed);
    }
    let title = title
        .map(|t| collapse_spaces(&t))
        .filter(|t| !t.is_empty())
        .or_else(|| page_title(&doc, url))?;
    Some(BookFacts {
        isbn: isbn?,
        title,
        authors: author.map(|a| collapse_spaces(&a)).unwrap_or_default(),
    })
}

fn most_frequent(items: Vec<String>) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for item in items {
        match counts.iter_mut().find(|(i, _)| *i == item) {
            Some((_, n)) => *n += 1,
            None => counts.push((item, 1)),
        }
    }
    // The first one seen wins a tie.
    let max = counts.iter().map(|(_, n)| *n).max()?;
    counts.into_iter().find(|(_, n)| *n == max).map(|(i, _)| i)
}

/// Whether a title candidate is the site's name ("Honoré Champion" on
/// honorechampion.com, "Éditions Verdier" on editions-verdier.fr): an
/// address built on the site's name matches it better than the book's title.
pub fn names_the_site(words: &[&str], url: &Url) -> bool {
    const HOUSE: [&str; 3] = ["les", "editions", "edition"];
    let squeeze = |words: &[&str]| -> String {
        words
            .iter()
            .skip_while(|w| HOUSE.contains(w))
            .copied()
            .collect()
    };
    let host = bare_host(url);
    let label = host.split('.').next().unwrap_or("");
    let label_words: Vec<&str> = label.split('-').collect();
    let name = squeeze(words);
    !name.is_empty() && name == squeeze(&label_words)
}

/// The page's title as the book's title: among every `<h1>`, `og:title`,
/// `<title>` and their parts split on ` - `, ` | `, ` – `, `, `, the one whose
/// words best match the words of the page's URL, which almost always derives
/// from the title. Precision first, then how many words it covers. Real
/// cases: a first `<h1>` that is the site's banner, an `og:title` that adds
/// the author or names the section, a truncated one.
pub fn page_title(doc: &Html, url: &Url) -> Option<String> {
    let url_words: Vec<String> = fold(&decoded_path(url))
        .split(' ')
        .map(str::to_owned)
        .collect();
    let mut raw: Vec<String> = doc.select(&H1).map(|h| element_text(&h)).collect();
    if let Some(og) = doc
        .select(&OG_TITLE)
        .find_map(|m| m.value().attr("content"))
    {
        raw.push(og.to_owned());
    }
    if let Some(t) = doc.select(&TITLE).next() {
        raw.push(element_text(&t));
    }
    let mut candidates: Vec<String> = Vec::new();
    for t in raw {
        let t = collapse_spaces(&t);
        if t.is_empty() {
            continue;
        }
        candidates.push(t.clone());
        candidates.extend(split_title_parts(&t));
    }
    let mut best: Option<(String, (f64, usize))> = None;
    let mut seen: Vec<&String> = Vec::new();
    for c in &candidates {
        if seen.contains(&c) {
            continue;
        }
        seen.push(c);
        let folded = fold(c);
        let words: Vec<&str> = folded.split(' ').filter(|w| !w.is_empty()).collect();
        if words.is_empty() || names_the_site(&words, url) {
            continue;
        }
        let hit = words
            .iter()
            .filter(|w| url_words.iter().any(|u| u == *w))
            .count();
        let key = (hit as f64 / words.len() as f64, hit);
        if hit > 0 && best.as_ref().is_none_or(|(_, k)| key > *k) {
            best = Some((c.clone(), key));
        }
    }
    best.map(|(t, _)| t).or_else(|| {
        doc.select(&H1)
            .next()
            .map(|h| collapse_spaces(&element_text(&h)))
            .filter(|t| !t.is_empty())
    })
}

/// The parts of a page title split on its separators.
fn split_title_parts(t: &str) -> Vec<String> {
    const MARK: char = '\u{1f}';
    let mut s = t.to_owned();
    for sep in [" | ", " – ", " — ", " - ", " , ", ", "] {
        s = s.replace(sep, &MARK.to_string());
    }
    s.split(MARK)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Where a page states an ISBN: its visible text, its JSON-LD, and its
/// ISBN, GTIN or EAN metadata. Not its attributes or other scripts, which
/// echo the address asked (canonical, og:url, hreflang, a search box's
/// value): a "not found" page would then pass for the book's.
fn isbn_evidence(doc: &Html) -> String {
    let mut out = String::new();
    for node in doc.root_element().descendants() {
        let Some(text) = node.value().as_text() else {
            continue;
        };
        let hidden = node.ancestors().any(|a| {
            a.value().as_element().is_some_and(|e| {
                matches!(
                    e.name(),
                    "script" | "style" | "noscript" | "template" | "title"
                )
            })
        });
        if !hidden {
            out.push_str(text);
            out.push(' ');
        }
    }
    for script in doc.select(&JSON_LD).filter(|s| {
        s.value()
            .attr("type")
            .is_some_and(|t| t.to_ascii_lowercase().contains("ld+json"))
    }) {
        out.extend(script.text());
        out.push(' ');
    }
    for meta in doc.select(&META_CONTENT) {
        let e = meta.value();
        let names = [e.attr("property"), e.attr("name"), e.attr("itemprop")];
        let about_isbn = names.iter().flatten().any(|n| {
            let n = n.to_ascii_lowercase();
            ["isbn", "gtin", "ean"].iter().any(|k| n.contains(k))
        });
        if about_isbn {
            out.push_str(e.attr("content").unwrap_or(""));
            out.push(' ');
        }
    }
    out
}

/// Whether the page is the book's: it states the ISBN (13 or 10 digits,
/// printed with or without separators, see [`isbn_evidence`]), or, when
/// `title` is not empty, its headings hold at least 80 % of the title's
/// words.
pub fn page_matches(html: &str, isbn: &str, title: &str) -> Option<PageMatch> {
    let doc = Html::parse_document(html);
    let flat = strip_isbn_separators(&isbn_evidence(&doc));
    if !isbn.is_empty()
        && (flat.contains(isbn) || to_isbn10(isbn).is_some_and(|i| flat.contains(&i)))
    {
        return Some(PageMatch::Isbn);
    }
    let words = title_words(title);
    if words.is_empty() {
        return None;
    }
    let heads = doc
        .select(&HEADS)
        .map(|e| element_text(&e))
        .collect::<Vec<_>>()
        .join(" ");
    let heads = fold(&heads);
    let heads: Vec<&str> = heads.split(' ').collect();
    let found = words.iter().filter(|w| heads.contains(&w.as_str())).count();
    (found * 5 >= words.len() * 4).then_some(PageMatch::Title)
}

/// Words of an id or class that mark the site's frame.
const CHROME_WORDS: [&str; 4] = ["logo", "navbar", "menu", "footer"];

/// Whether an element's id or classes mark the site's frame: one of its
/// names holds [`CHROME_WORDS`] as a whole word (`main-menu`, `menu-item`,
/// `custom-logo-link`), or is a `site-header`.
fn marks_chrome(e: &scraper::node::Element) -> bool {
    let id = e.attr("id").unwrap_or("");
    let classes = e.attr("class").unwrap_or("");
    id.split_whitespace()
        .chain(classes.split_whitespace())
        .any(|name| {
            let name = name.to_ascii_lowercase();
            name.contains("site-header")
                || name
                    .split(['-', '_'])
                    .any(|part| CHROME_WORDS.contains(&part))
        })
}

/// Whether a link sits in the site's frame (header, menu, footer, logo)
/// rather than in the page's content. `<body>` and `<html>` are not looked
/// at: themes put site-wide classes there (WordPress adds `wp-custom-logo`
/// to `<body>` whenever the theme has a logo).
fn in_site_chrome(link: &ElementRef) -> bool {
    for ancestor in link.ancestors() {
        let Some(e) = ancestor.value().as_element() else {
            continue;
        };
        if matches!(e.name(), "body" | "html") {
            break;
        }
        if matches!(e.name(), "header" | "nav" | "footer") || marks_chrome(e) {
            return true;
        }
    }
    let own = link.value();
    own.attr("id")
        .into_iter()
        .chain(own.attr("class"))
        .any(|v| {
            v.to_ascii_lowercase()
                .split(['-', '_', ' '])
                .any(|p| p == "logo")
        })
}

/// The links of `html` (fetched from `page_url`) resolved, without fragment,
/// on the same site, out of the site's frame.
fn content_links<'a>(
    doc: &'a Html,
    page_url: &'a Url,
) -> impl Iterator<Item = (Url, ElementRef<'a>)> + 'a {
    site_links(doc, page_url).filter(|(_, a)| !in_site_chrome(a))
}

/// The links of `html` (fetched from `page_url`) resolved, without fragment,
/// on the same site, the site's frame included.
fn site_links<'a>(
    doc: &'a Html,
    page_url: &'a Url,
) -> impl Iterator<Item = (Url, ElementRef<'a>)> + 'a {
    let host = bare_host(page_url);
    doc.select(&LINKS).filter_map(move |a| {
        let mut url = page_url.join(a.value().attr("href")?.trim()).ok()?;
        url.set_fragment(None);
        let ok = matches!(url.scheme(), "http" | "https") && same_site(&url, &host);
        ok.then_some((url, a))
    })
}

/// The links of a search result page that lead to the wanted book, best
/// first. A link with the ISBN in its path comes first; the ISBN in a query
/// string does not count (the result page's own filters carry it). Without
/// it, at least 60 % of the title's words must be in the link's path, text or
/// title, the author's last name adding to the score and a listing page
/// (author, collection) taking from it.
pub fn result_links(
    html: &str,
    page_url: &Url,
    isbn: &str,
    title: &str,
    authors: &str,
) -> Vec<Url> {
    let doc = Html::parse_document(html);
    let words = title_words(title);
    let last_name = authors
        .split(',')
        .next()
        .map(fold)
        .and_then(|a| a.split(' ').next_back().map(str::to_owned))
        .filter(|a| !a.is_empty());
    let isbn10 = to_isbn10(isbn);
    let search_path = page_url.path().trim_end_matches('/').to_owned();

    let mut scored: Vec<(Url, i32)> = Vec::new();
    for (url, a) in content_links(&doc, page_url) {
        let path = url.path().to_owned();
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed == search_path {
            continue;
        }
        let score = if !isbn.is_empty()
            && (path.contains(isbn) || isbn10.as_deref().is_some_and(|i| path.contains(i)))
        {
            100
        } else if !words.is_empty() {
            let hay = fold(&format!(
                "{} {} {}",
                decoded_path(&url),
                element_text(&a),
                a.value().attr("title").unwrap_or("")
            ));
            let hay: Vec<&str> = hay.split(' ').collect();
            let found = words.iter().filter(|w| hay.contains(&w.as_str())).count();
            if found * 5 < words.len() * 3 {
                continue;
            }
            let mut score = (50 * found / words.len()) as i32;
            if last_name.as_deref().is_some_and(|l| hay.contains(&l)) {
                score += 10;
            }
            if LISTING_HINTS.iter().any(|k| path.contains(k)) {
                score -= 30;
            }
            score
        } else {
            continue;
        };
        match scored.iter_mut().find(|(u, _)| *u == url) {
            Some((_, s)) => *s = (*s).max(score),
            None => scored.push((url, score)),
        }
    }
    // Stable: equal scores keep the page's order.
    scored.sort_by_key(|(_, s)| -s);
    scored.into_iter().map(|(u, _)| u).collect()
}

/// How much a path looks like a book page's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BookPath {
    /// It carries an ISBN.
    Isbn,
    /// A book-like word and a slug: `/livre/ma-reine`, `/fiche-livre-ma-reine`.
    Slug,
    /// A book-like word above a bare name: `/Romans/Doigtsperdus.html`.
    Name,
}

fn book_path(path: &str) -> Option<BookPath> {
    if !find_isbns(path).is_empty() {
        return Some(BookPath::Isbn);
    }
    let lower = path.trim_end_matches('/').to_ascii_lowercase();
    let (above, last) = lower.rsplit_once('/')?;
    let hinted = |s: &str| PRODUCT_HINTS.iter().any(|h| s.contains(h));
    let words = last.split(['-', '_']).filter(|w| !w.is_empty()).count();
    if hinted(above) {
        Some(if words >= 2 {
            BookPath::Slug
        } else {
            BookPath::Name
        })
    } else if hinted(last) && words >= MIN_NAMED_SEGMENT_WORDS {
        Some(BookPath::Slug)
    } else {
        None
    }
}

/// Among `urls`, those that probably lead to books: with an ISBN in their
/// path first, then with a book-like word and a slug, then with a book-like
/// word above a bare name. In the order given, each once.
pub fn announced_books(urls: &[Url]) -> Vec<Url> {
    let mut ranked: Vec<(&Url, BookPath)> = Vec::new();
    let mut seen: HashSet<&Url> = HashSet::new();
    for url in urls {
        if let Some(rank) = book_path(url.path())
            && seen.insert(url)
        {
            ranked.push((url, rank));
        }
    }
    [BookPath::Isbn, BookPath::Slug, BookPath::Name]
        .into_iter()
        .flat_map(|rank| {
            ranked
                .iter()
                .filter(move |(_, r)| *r == rank)
                .map(|(u, _)| (*u).clone())
        })
        .collect()
}

/// Among `urls`, those nothing tells apart: no book-like path (see
/// [`announced_books`]), not the home page, a file or a listing. In the order
/// given, each once.
pub fn unannounced_pages(urls: &[Url]) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    let mut seen: HashSet<&Url> = HashSet::new();
    for url in urls {
        let path = url.path().to_ascii_lowercase();
        let skip = path.trim_end_matches('/').is_empty()
            || book_path(&path).is_some()
            || FILE_EXTENSIONS.iter().any(|e| path.ends_with(e))
            || LISTING_HINTS.iter().any(|k| path.contains(k));
        if !skip && seen.insert(url) {
            out.push(url.clone());
        }
    }
    out
}

/// The content links of a page, without query or fragment, in page order.
/// A page with none has its content inside what we take for the frame (a
/// template wrapping everything in `<header>`): all its links are given.
fn plain_content_links(html: &str, page_url: &Url) -> Vec<Url> {
    let doc = Html::parse_document(html);
    let plain = |(mut url, _): (Url, ElementRef)| {
        url.set_query(None);
        url
    };
    let content: Vec<Url> = content_links(&doc, page_url).map(plain).collect();
    if content.is_empty() {
        site_links(&doc, page_url).map(plain).collect()
    } else {
        content
    }
}

/// Links of a page that probably lead to books (see [`announced_books`]).
pub fn witness_links(html: &str, page_url: &Url) -> Vec<Url> {
    announced_books(&plain_content_links(html, page_url))
}

/// The content links of a page that nothing tells apart (see
/// [`unannounced_pages`]).
pub fn other_links(html: &str, page_url: &Url) -> Vec<Url> {
    unannounced_pages(&plain_content_links(html, page_url))
}

/// A site map read as text: whether it is an index of site maps, and the
/// addresses it lists that are on the site of `site_url`, without fragment,
/// in the file's order, each once. Read by its `<loc>` elements, so a file cut
/// short still gives what came before the cut.
pub fn sitemap_locs(xml: &str, site_url: &Url) -> (bool, Vec<Url>) {
    let host = bare_host(site_url);
    let is_index = xml.contains("<sitemapindex");
    let mut out: Vec<Url> = Vec::new();
    let mut seen: HashSet<Url> = HashSet::new();
    for chunk in xml.split("<loc>").skip(1) {
        let Some((loc, _)) = chunk.split_once("</loc>") else {
            continue;
        };
        let loc = loc.trim();
        let loc = loc
            .strip_prefix("<![CDATA[")
            .and_then(|l| l.strip_suffix("]]>"))
            .unwrap_or(loc)
            .trim()
            .replace("&amp;", "&");
        let Ok(mut url) = Url::parse(&loc) else {
            continue;
        };
        url.set_fragment(None);
        if matches!(url.scheme(), "http" | "https")
            && same_site(&url, &host)
            && seen.insert(url.clone())
        {
            out.push(url);
        }
    }
    (is_index, out)
}

/// Share of a title's words, in fifths, an address must carry to be taken
/// for the book's page in a site map.
const SITEMAP_TITLE_FIFTHS: usize = 4;

/// Among the `pages` a site map lists, those that may be the book's, best
/// first: with the ISBN in their path, then named after the title (at least
/// four fifths of its words, apostrophes read both ways: `l-hiver` and
/// `lhiver`), the address with the fewest other words first. Equal pages
/// keep the map's order.
pub fn sitemap_book_links(pages: &[Url], isbn: &str, title: &str) -> Vec<Url> {
    let isbn10 = to_isbn10(isbn);
    let apostrophe = ['\'', '\u{2019}'];
    let spellings: Vec<Vec<String>> = [
        title_words(&title.replace(apostrophe, " ")),
        title_words(&title.replace(apostrophe, "")),
    ]
    .into_iter()
    .filter(|words| !words.is_empty())
    .collect();
    // (0, 0) for the ISBN, else (1, words of the address not in the title).
    let mut ranked: Vec<((usize, usize), &Url)> = Vec::new();
    for url in pages {
        let path = url.path();
        if !isbn.is_empty()
            && (path.contains(isbn) || isbn10.as_deref().is_some_and(|i| path.contains(i)))
        {
            ranked.push(((0, 0), url));
            continue;
        }
        if spellings.is_empty() {
            continue;
        }
        let folded = fold(&decoded_path(url));
        let hay: Vec<&str> = folded.split(' ').collect();
        let others = spellings
            .iter()
            .filter_map(|words| {
                let found = words.iter().filter(|w| hay.contains(&w.as_str())).count();
                (found * 5 >= words.len() * SITEMAP_TITLE_FIFTHS)
                    .then(|| hay.len().saturating_sub(found))
            })
            .min();
        if let Some(others) = others {
            ranked.push(((1, others), url));
        }
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, u)| u.clone()).collect()
}

/// Links to the page listing the publisher's books, the most telling name
/// first (see [`CATALOGUE_WORDS`]), then the shortest address. Menus included:
/// that is where such a link sits.
pub fn catalogue_links(html: &str, page_url: &Url) -> Vec<Url> {
    let doc = Html::parse_document(html);
    let host = bare_host(page_url);
    let mut ranked: Vec<(Url, usize)> = Vec::new();
    for a in doc.select(&LINKS) {
        let Some(mut url) = a
            .value()
            .attr("href")
            .and_then(|h| page_url.join(h.trim()).ok())
        else {
            continue;
        };
        url.set_fragment(None);
        url.set_query(None);
        if !matches!(url.scheme(), "http" | "https") || !same_site(&url, &host) {
            continue;
        }
        let path = decoded_path(&url);
        let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let named = fold(&format!("{last} {}", element_text(&a)));
        let Some(rank) = CATALOGUE_WORDS
            .iter()
            .position(|w| named.split(' ').any(|n| n == *w))
        else {
            continue;
        };
        match ranked.iter_mut().find(|(u, _)| *u == url) {
            Some((_, r)) => *r = (*r).min(rank),
            None => ranked.push((url, rank)),
        }
    }
    // Stable: equal links keep the page's order.
    ranked.sort_by_key(|(u, rank)| (*rank, u.path().len()));
    ranked.into_iter().map(|(u, _)| u).collect()
}

/// How much an input looks like a search box; zero or less is not one.
fn search_input_score(input: &scraper::node::Element) -> i32 {
    let name = input.attr("name").unwrap_or("").to_ascii_lowercase();
    let kind = input.attr("type").unwrap_or("text").to_ascii_lowercase();
    if kind != "text" && kind != "search" {
        return 0;
    }
    let ident = [
        name.as_str(),
        input.attr("id").unwrap_or(""),
        input.attr("placeholder").unwrap_or(""),
        input.attr("aria-label").unwrap_or(""),
        input.attr("class").unwrap_or(""),
    ]
    .join(" ")
    .to_lowercase();
    let mut score = 0;
    if kind == "search" {
        score += 3;
    }
    if SEARCH_NAMES.contains(&name.as_str()) {
        score += 3;
    }
    if ["search", "recherch", "cherch", "query"]
        .iter()
        .any(|k| ident.contains(k))
    {
        score += 2;
    }
    if ["mail", "newsletter", "login", "password", "user"]
        .iter()
        .any(|k| ident.contains(k))
    {
        score -= 10;
    }
    score
}

/// The forms of a page that look like a search, with every field they send.
pub fn search_forms(html: &str, page_url: &Url) -> Vec<SearchForm> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for form in doc.select(&FORMS) {
        let inputs: Vec<ElementRef> = form.select(&INPUTS).collect();
        // The first input with the best score.
        let mut best: Option<(i32, &ElementRef)> = None;
        for input in &inputs {
            let score = search_input_score(input.value());
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, input));
            }
        }
        let Some((score, query_input)) = best else {
            continue;
        };
        if score <= 0 {
            continue;
        }
        let action = form.value().attr("action").filter(|a| !a.trim().is_empty());
        let Ok(mut action) = action.map_or(Ok(page_url.clone()), |a| page_url.join(a.trim()))
        else {
            continue;
        };
        let post = form
            .value()
            .attr("method")
            .is_some_and(|m| m.eq_ignore_ascii_case("post"));
        let mut fields: Vec<(String, String)> = Vec::new();
        if !post {
            fields.extend(
                action
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned())),
            );
            action.set_query(None);
        }
        for hidden in inputs.iter().filter(|i| {
            i.value()
                .attr("type")
                .is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
        }) {
            let name = hidden.value().attr("name").unwrap_or("").to_owned();
            set_field(
                &mut fields,
                name,
                hidden.value().attr("value").unwrap_or("").to_owned(),
            );
        }
        let query_name = query_input.value().attr("name").unwrap_or("").to_owned();
        set_field(&mut fields, query_name, "{q}".to_owned());
        let form = SearchForm {
            post,
            action,
            fields,
        };
        if !out.contains(&form) {
            out.push(form);
        }
    }
    out
}

fn set_field(fields: &mut Vec<(String, String)>, name: String, value: String) {
    match fields.iter_mut().find(|(n, _)| *n == name) {
        Some((_, v)) => *v = value,
        None => fields.push((name, value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn book_facts_prefer_json_ld() {
        let html = r#"<html><head><title>Ma reine | Éditions de l'Iconoclaste</title>
            <script type="application/ld+json">{"@context":"https://schema.org","@graph":[
              {"@type":"WebPage","name":"x"},
              {"@type":"Book","name":"Ma reine","isbn":"978-2-37880-296-7",
               "author":[{"@type":"Person","name":"Jean-Baptiste Andrea"}]}]}</script>
            </head><body><h1>Ma reine</h1></body></html>"#;
        let facts = book_facts(
            html,
            &url("https://editions-iconoclaste.fr/livre/ma-reine/"),
        )
        .unwrap();
        assert_eq!(
            facts,
            BookFacts {
                isbn: "9782378802967".into(),
                title: "Ma reine".into(),
                authors: "Jean-Baptiste Andrea".into()
            }
        );
    }

    #[test]
    fn book_facts_fall_back_on_the_url_then_the_text() {
        let html = "<h1>Odile Jacob publie</h1><h1>Le Cerveau</h1><p>EAN 978 2 7381 1234 5</p>";
        let page = url("https://www.odilejacob.fr/catalogue/le-cerveau_9782738112345.php");
        let facts = book_facts(html, &page).unwrap();
        assert_eq!(facts.isbn, "9782738112345");
        assert_eq!(facts.title, "Le Cerveau");

        let page = url("https://www.example-editions.fr/livre/le-cerveau");
        assert_eq!(book_facts(html, &page).unwrap().isbn, "9782738112345");
        assert!(book_facts("<h1>Accueil</h1>", &page).is_none());
    }

    #[test]
    fn page_title_is_not_the_site_name() {
        let html = r#"<head><title>Honoré Champion</title>
            <meta property="og:title" content="UN LIVRE DE FAMILLE - JEAN BOUTIER"></head>
            <body><h1>UN LIVRE DE FAMILLE</h1></body>"#;
        let doc = Html::parse_document(html);
        let page = url(
            "https://www.honorechampion.com/fr/editions-honore-champion/13531-book-08536565-9782745365651.html",
        );
        assert_eq!(
            page_title(&doc, &page).as_deref(),
            Some("UN LIVRE DE FAMILLE")
        );
        let verdier = url("https://editions-verdier.fr/livre/editions-verdier/");
        assert!(names_the_site(&["editions", "verdier"], &verdier));
        assert!(!names_the_site(&["le", "grand", "voyage"], &verdier));
    }

    #[test]
    fn page_title_matches_the_url_words() {
        let html = r#"<head><meta property="og:title" content="Les Fiancés de l'hiver de Christelle Dabos">
            <title>Les Fiancés de l'hiver - Gallimard Jeunesse</title></head>"#;
        let doc = Html::parse_document(html);
        let page = url("https://www.gallimard.fr/catalogue/les-fiances-de-l-hiver/9782070000000");
        assert_eq!(
            page_title(&doc, &page).as_deref(),
            Some("Les Fiancés de l'hiver")
        );
    }

    #[test]
    fn page_matches_by_isbn_or_title() {
        assert_eq!(
            page_matches("<p>ISBN 978-2-07-012345-2</p>", "9782070123452", ""),
            Some(PageMatch::Isbn)
        );
        // The 10-digit form of the same ISBN.
        let isbn10 = to_isbn10("9782070123452").unwrap();
        assert_eq!(
            page_matches(&format!("<p>{isbn10}</p>"), "9782070123452", ""),
            Some(PageMatch::Isbn)
        );
        let html = "<title>Le Palais des miroirs</title><h1>Le Palais des miroirs</h1>";
        assert_eq!(
            page_matches(html, "9782070000000", "Le Palais des miroirs"),
            Some(PageMatch::Title)
        );
        assert_eq!(
            page_matches(html, "9782070000000", "Le Palais des glaces"),
            None
        );
    }

    #[test]
    fn an_isbn_echoed_in_attributes_is_not_the_book() {
        // A "not found" page that echoes the address asked.
        let html = r#"<head><link rel="canonical" href="https://e.example/livre/x/9782070123452">
            <meta property="og:url" content="https://e.example/livre/x/9782070123452"></head>
            <body><form><input name="q" value="9782070123452"></form><h1>Page introuvable</h1>
            <script>var asked = "/livre/x/9782070123452";</script></body>"#;
        assert_eq!(page_matches(html, "9782070123452", ""), None);
        // Stated in metadata or JSON-LD, it counts.
        let meta = r#"<meta property="book:isbn" content="9782070123452"><h1>Le Livre</h1>"#;
        assert_eq!(
            page_matches(meta, "9782070123452", ""),
            Some(PageMatch::Isbn)
        );
        let json_ld = r#"<script type="application/ld+json">{"@type":"Book","isbn":"9782070123452"}</script>"#;
        assert_eq!(
            page_matches(json_ld, "9782070123452", ""),
            Some(PageMatch::Isbn)
        );
    }

    #[test]
    fn a_site_wide_body_class_does_not_hide_every_link() {
        let html = r#"<html class="menu-open"><body class="home wp-custom-logo">
            <div class="site-branding"><a class="custom-logo-link" href="/">Logo</a></div>
            <ul class="main-menu"><li><a href="/livre/menu-9782000000028">Menu</a></li></ul>
            <main><a href="/livre/le-grand-voyage-9782000000011">Le Grand Voyage</a></main></body></html>"#;
        let links = witness_links(html, &url("https://editions.example/"));
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(paths, ["/livre/le-grand-voyage-9782000000011"]);
    }

    #[test]
    fn result_links_rank_the_isbn_first_and_skip_filters() {
        let html = r#"<header><a href="/livre/ma-reine-9782378802967">menu</a></header>
            <a href="/recherche?search=9782378802967&f[0]=format">filtre</a>
            <a href="/auteur/andrea">Ma reine, tous les livres d'Andrea</a>
            <a href="/livres/ma-reine">Ma reine</a>
            <a href="/livre/ma-reine-9782378802967#avis">Ma reine</a>
            <a href="https://other.example/livre/ma-reine-9782378802967">ailleurs</a>"#;
        let page = url("https://www.editions.example/recherche?search=9782378802967");
        let links = result_links(
            html,
            &page,
            "9782378802967",
            "Ma reine",
            "Jean-Baptiste Andrea",
        );
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(
            paths,
            [
                "/livre/ma-reine-9782378802967",
                "/livres/ma-reine",
                "/auteur/andrea"
            ]
        );
    }

    #[test]
    fn witness_links_put_isbn_links_first() {
        let html = r#"<nav><a href="/livre/menu-9782000000001">x</a></nav>
            <a href="/livres/les-voyages-de-gulliver?ref=home">Gulliver</a>
            <a href="/catalogue/martin-eden/9782351788424">Martin Eden</a>
            <a href="/livres/les-voyages-de-gulliver">Gulliver</a>
            <a href="/contact">Contact</a>"#;
        let links = witness_links(html, &url("https://www.gallmeister.fr/"));
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(
            paths,
            [
                "/catalogue/martin-eden/9782351788424",
                "/livres/les-voyages-de-gulliver"
            ]
        );
    }

    #[test]
    fn witness_links_read_the_usual_book_paths() {
        let html = r#"
            <a href="/Romans/Doigtsperdus.html">Doigts perdus</a>
            <a href="/Essais.html">Essais</a>
            <a href="/nouvelles-parutions">Nouvelles parutions</a>
            <a href="/accueil/2050-les-reseaux-europeens.html">Les réseaux</a>
            <a href="/product/bon-marche/">Bon marché</a>
            <a href="/fiche-livre-serres-rose/">Rose</a>
            <a href="/catalogue-livres-jules.html">Jules</a>
            <a href="/collections/29-albiana-jeunesse">Jeunesse</a>"#;
        let links = witness_links(html, &url("https://editions.example/"));
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        // A slug with a dash first, then the bare names.
        assert_eq!(
            paths,
            [
                "/accueil/2050-les-reseaux-europeens.html",
                "/product/bon-marche/",
                "/fiche-livre-serres-rose/",
                "/catalogue-livres-jules.html",
                "/Romans/Doigtsperdus.html",
            ]
        );
    }

    #[test]
    fn catalogue_links_prefer_the_catalogue_then_the_shortest_address() {
        let html = r#"<nav><a href="/collections">Collections</a>
            <a href="/catalogue-livres-jules.html">Jules</a>
            <a href="/catalogue-livres.html">Catalogue</a>
            <a href="/tous">Nos livres</a>
            <a href="https://other.example/catalogue">Ailleurs</a></nav>
            <a href="/contact">Contact</a>"#;
        let links = catalogue_links(html, &url("https://editions.example/"));
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(
            paths,
            [
                "/catalogue-livres.html",
                "/catalogue-livres-jules.html",
                "/tous",
                "/collections"
            ]
        );
    }

    #[test]
    fn other_links_skip_files_listings_and_the_home_page() {
        let html = r#"<a href="/">Accueil</a>
            <a href="/Grand-Guignol">Grand-Guignol</a>
            <a href="/IMG/catalogue.pdf">PDF</a>
            <a href="/auteur/x">Auteur</a>
            <a href="/En-mer?lang=fr">En mer</a>
            <a href="/Grand-Guignol#extrait">Extrait</a>"#;
        let links = other_links(html, &url("https://editions.example/"));
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(paths, ["/Grand-Guignol", "/En-mer"]);
    }

    #[test]
    fn a_page_wholly_inside_its_header_still_gives_its_links() {
        let html = r#"<header><a href="/contact">Contact</a>
            <a href="/catalogue-livres-le-grand-voyage.html">Le Grand Voyage</a></header>"#;
        let links = witness_links(html, &url("https://editions.example/"));
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(paths, ["/catalogue-livres-le-grand-voyage.html"]);
    }

    #[test]
    fn sitemap_locs_keep_the_site_s_addresses() {
        let xml = r#"<?xml version="1.0"?><urlset>
            <url><loc>https://www.editions.example/livre/ma-reine</loc><image:image>
              <image:loc>https://cdn.example/ma-reine.jpg</image:loc></image:image></url>
            <url><loc><![CDATA[https://editions.example/recherche?a=1&amp;b=2]]></loc></url>
            <url><loc>https://other.example/livre/x</loc></url>
            <url><loc>https://editions.example/livre/cut"#;
        let (is_index, locs) = sitemap_locs(xml, &url("https://editions.example/"));
        assert!(!is_index);
        let locs: Vec<&str> = locs.iter().map(Url::as_str).collect();
        assert_eq!(
            locs,
            [
                "https://www.editions.example/livre/ma-reine",
                "https://editions.example/recherche?a=1&b=2"
            ]
        );
        let index = "<sitemapindex><sitemap><loc>https://editions.example/product-sitemap.xml</loc></sitemap></sitemapindex>";
        assert!(sitemap_locs(index, &url("https://editions.example/")).0);
    }

    #[test]
    fn sitemap_book_links_rank_the_isbn_then_the_closest_title() {
        let pages: Vec<Url> = [
            "https://e.example/auteur/l-hiver-des-loups-et-autres-recits",
            "https://e.example/boutique/romans/lhiver-des-loups/",
            "https://e.example/livre/9782000000028",
            "https://e.example/boutique/essais/le-printemps-des-loups/",
        ]
        .iter()
        .map(|u| url(u))
        .collect();
        let links = sitemap_book_links(&pages, "9782000000028", "L\u{2019}Hiver des loups");
        let paths: Vec<&str> = links.iter().map(Url::path).collect();
        assert_eq!(
            paths,
            [
                "/livre/9782000000028",
                "/boutique/romans/lhiver-des-loups/",
                "/auteur/l-hiver-des-loups-et-autres-recits",
            ]
        );
        // A title without a word worth comparing is found by its ISBN only.
        assert!(sitemap_book_links(&pages, "9782000000035", "Ça").is_empty());
    }

    #[test]
    fn a_page_printing_many_isbns_is_a_listing() {
        let page = url("https://editions.example/Essais");
        let one = "<h1>Essais</h1><p>ISBN 978-2-7381-1234-5</p>";
        assert!(single_book_facts(one, &page).is_some());
        let many: String = (0..6)
            .map(|i| format!("<p>ISBN 978200000{i:04}</p>"))
            .collect();
        let many = format!("<h1>Essais</h1>{many}");
        assert!(book_facts(&many, &page).is_some());
        assert!(single_book_facts(&many, &page).is_none());
    }

    #[test]
    fn search_forms_keep_hidden_fields_and_skip_newsletters() {
        let html = r#"
            <form action="/newsletter" method="post"><input type="email" name="email"><input name="nl_name" placeholder="Votre e-mail"></form>
            <form action="/catalogue?view=list"><input type="hidden" name="lang" value="fr">
              <input type="text" name="s[q]" placeholder="Rechercher"></form>
            <form action="index.php?page=recherche" method="POST"><input name="quoi" placeholder="Recherche"></form>"#;
        let forms = search_forms(html, &url("https://actes-sud.example/"));
        assert_eq!(forms.len(), 2);
        assert!(!forms[0].post);
        assert_eq!(
            forms[0].action.as_str(),
            "https://actes-sud.example/catalogue"
        );
        assert_eq!(
            forms[0].fields,
            [
                ("view".into(), "list".into()),
                ("lang".into(), "fr".into()),
                ("s[q]".into(), "{q}".into())
            ]
        );
        // A POST form keeps its action's query, which the server reads there.
        assert!(forms[1].post);
        assert_eq!(
            forms[1].action.as_str(),
            "https://actes-sud.example/index.php?page=recherche"
        );
    }
}
