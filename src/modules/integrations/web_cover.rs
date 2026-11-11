//! Pick a book cover from a web page the user points at.
//!
//! The open sources do not carry every book: recent French titles from small
//! publishers are the usual gap. Their cover is on the publisher's own page, so
//! the user pastes that page's address (or the image's) and this module finds
//! the image. It reads the hints a page publishes for link previews and search
//! engines, most specific first:
//!
//! 1. the `image` of a schema.org `Book` in a JSON-LD block, then of a
//!    `Product` carrying an ISBN or GTIN (how bookshops describe a book);
//! 2. `og:image` (Open Graph), then `twitter:image`;
//! 3. `<link rel="image_src">`;
//! 4. the `image` of any other `Product`;
//! 5. the page's `<img>` tags, as a last resort the user steps through.
//!
//! When the book's ISBN is known, images whose address contains it go first:
//! bookshops name their cover files after the EAN, which tells the cover from
//! a site banner even on a page that declares nothing.
//!
//! Every request goes through `utils::public_fetch` (public https only, checked
//! after DNS resolution and on every redirect, bounded size and time). The image
//! format is checked here from its magic bytes; decoding, orientation and
//! resizing happen in the app's cover photo pipeline, so a web cover and a
//! photographed one end up as the same kind of file.

use std::sync::LazyLock;
use std::time::Duration;

use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::utils::cover_image::COVER_MAX_INPUT_BYTES;
use crate::utils::isbn;
use crate::utils::public_fetch::{self, BodyCap, FetchPolicy, PublicFetchError, upgrade_to_https};

/// Cap on an HTML page. A publisher's book page is far below this; the cap
/// only bounds what a hostile or broken server can make us hold in memory.
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// Candidates kept, best first: tried in order until one is a usable image
/// (the first may be gone or a format we cannot use while the next one is
/// the cover), then handed back for the user to step through ("next image").
/// Bounded so a page cannot make us fire a request per tag.
pub const MAX_LISTED_CANDIDATES: usize = 12;

/// An `<img>` declaring a smaller side than this is a pixel, an icon or a
/// logo, not a cover.
const MIN_IMG_SIDE: u32 = 100;

/// JSON-LD nesting walked before giving up on a block.
const MAX_JSON_LD_DEPTH: usize = 8;

/// A cover found on the web: the raw image and the address it was found at.
#[derive(Debug)]
pub struct WebCover {
    pub bytes: Vec<u8>,
    /// The image URL as the page declared it (or as the user pasted it), not
    /// the post-redirect one: a CDN's redirect target is often short-lived.
    pub image_url: String,
    /// Every usable candidate of the page, best first, `image_url` included,
    /// for the user to step through. Empty for a single fetched candidate.
    pub candidates: Vec<String>,
}

/// Why no cover came out of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebCoverError {
    Fetch(PublicFetchError),
    /// The page answered but declares no usable image.
    NoImageFound,
    /// The image is in a format the cover pipeline cannot decode.
    UnsupportedImage,
}

impl WebCoverError {
    /// A stable code for the app to map to a translated message.
    pub fn code(&self) -> &'static str {
        match self {
            WebCoverError::Fetch(PublicFetchError::InvalidUrl) => "invalid_url",
            WebCoverError::Fetch(PublicFetchError::BlockedAddress) => "blocked_address",
            WebCoverError::Fetch(PublicFetchError::Network) => "network",
            WebCoverError::Fetch(PublicFetchError::TooManyRedirects) => "too_many_redirects",
            WebCoverError::Fetch(PublicFetchError::HttpStatus(_)) => "http_status",
            WebCoverError::Fetch(PublicFetchError::TooLarge) => "too_large",
            WebCoverError::NoImageFound => "no_image_found",
            WebCoverError::UnsupportedImage => "unsupported_image",
        }
    }
}

impl From<PublicFetchError> for WebCoverError {
    fn from(e: PublicFetchError) -> Self {
        WebCoverError::Fetch(e)
    }
}

/// Fetch the cover of the book shown at `raw_url`, which may be the book's
/// page or the image itself. `isbn`, when known, ranks the images named after
/// it first.
pub async fn fetch_cover_from_page(
    raw_url: &str,
    isbn: Option<&str>,
) -> Result<WebCover, WebCoverError> {
    let url = public_fetch::parse_public_https_url(raw_url)?;
    fetch_cover_with_policy(
        url,
        FetchPolicy::PublicInternet,
        PICK_DEADLINE,
        isbn.map(str::to_owned),
    )
    .await
}

/// Fetch one candidate the user stepped to. The address comes back from the
/// app, so it goes through the same checks as any other.
pub async fn fetch_cover_image(raw_url: &str) -> Result<WebCover, WebCoverError> {
    let url = public_fetch::parse_public_https_url(raw_url)?;
    fetch_image_with_policy(url, FetchPolicy::PublicInternet).await
}

pub(crate) async fn fetch_image_with_policy(
    url: Url,
    policy: FetchPolicy,
) -> Result<WebCover, WebCoverError> {
    let fetch = public_fetch::fetch_with_policy(
        url.clone(),
        &|_| BodyCap::Refuse(COVER_MAX_INPUT_BYTES),
        policy,
    );
    let image = tokio::time::timeout(PICK_DEADLINE, fetch)
        .await
        .unwrap_or(Err(PublicFetchError::Network))?;
    check_usable_image(&image.bytes)?;
    Ok(WebCover {
        bytes: image.bytes,
        image_url: url.to_string(),
        candidates: Vec::new(),
    })
}

/// Ceiling on a whole pick (page, redirects, up to [`MAX_LISTED_CANDIDATES`]
/// images). The app's dialog cannot be dismissed while it runs, so a slow or
/// hostile server must not hold it longer than this. Leaves room for a 10 MB
/// image over a 1 Mbit/s link.
pub const PICK_DEADLINE: Duration = Duration::from_secs(120);

pub(crate) async fn fetch_cover_with_policy(
    url: Url,
    policy: FetchPolicy,
    deadline: Duration,
    isbn: Option<String>,
) -> Result<WebCover, WebCoverError> {
    tokio::time::timeout(deadline, pick_cover(url, policy, isbn))
        .await
        .unwrap_or(Err(PublicFetchError::Network.into()))
}

async fn pick_cover(
    url: Url,
    policy: FetchPolicy,
    isbn: Option<String>,
) -> Result<WebCover, WebCoverError> {
    let page = public_fetch::fetch_with_policy(url.clone(), &body_cap, policy).await?;
    if sniff_image(&page.bytes).is_some() {
        check_usable_image(&page.bytes)?;
        return Ok(WebCover {
            bytes: page.bytes,
            image_url: url.to_string(),
            candidates: vec![url.to_string()],
        });
    }
    if page
        .content_type
        .as_deref()
        .is_some_and(|ct| ct.starts_with("image/"))
    {
        // An image, but not one we can decode (SVG, AVIF...).
        return Err(WebCoverError::UnsupportedImage);
    }
    // An untyped body was read up to the image cap; as a page, only its first
    // MAX_PAGE_BYTES are parsed, like a typed one.
    let head = &page.bytes[..page.bytes.len().min(MAX_PAGE_BYTES)];

    // Parsing a page of up to MAX_PAGE_BYTES is CPU work: keep it off the
    // async runtime's worker threads.
    let html = String::from_utf8_lossy(head).into_owned();
    let page_url = page.final_url;
    let candidates =
        tokio::task::spawn_blocking(move || extract_cover_candidates(&html, &page_url))
            .await
            .map_err(|_| WebCoverError::NoImageFound)?;
    let usable: Vec<Url> = candidates
        .into_iter()
        .filter_map(|candidate| {
            let candidate = if policy.upgrades_http() {
                upgrade_to_https(candidate)?
            } else {
                candidate
            };
            policy.check(&candidate).ok().map(|()| candidate)
        })
        .collect();
    let usable: Vec<Url> = rank_by_isbn(usable, isbn.as_deref())
        .into_iter()
        .take(MAX_LISTED_CANDIDATES)
        .collect();
    let candidates: Vec<String> = usable.iter().map(Url::to_string).collect();

    // The last failure is reported when none works.
    let mut error = WebCoverError::NoImageFound;
    for image_url in usable {
        match public_fetch::fetch_with_policy(
            image_url.clone(),
            &|_| BodyCap::Refuse(COVER_MAX_INPUT_BYTES),
            policy,
        )
        .await
        {
            Ok(image) if check_usable_image(&image.bytes).is_ok() => {
                return Ok(WebCover {
                    bytes: image.bytes,
                    image_url: image_url.to_string(),
                    candidates,
                });
            }
            Ok(image) => {
                error = check_usable_image(&image.bytes)
                    .err()
                    .unwrap_or(WebCoverError::UnsupportedImage)
            }
            Err(e) => error = e.into(),
        }
    }
    Err(error)
}

/// Move the candidates whose address contains the book's ISBN (either length)
/// to the front, keeping the page order otherwise. No ISBN, or one that does
/// not parse, leaves the order as it is.
fn rank_by_isbn(candidates: Vec<Url>, isbn: Option<&str>) -> Vec<Url> {
    let Some(isbn) = isbn else {
        return candidates;
    };
    let forms: Vec<String> = [isbn::to_isbn13(isbn), isbn::to_isbn10(isbn)]
        .into_iter()
        .flatten()
        .collect();
    if forms.is_empty() {
        return candidates;
    }
    let (named, others): (Vec<Url>, Vec<Url>) = candidates
        .into_iter()
        .partition(|u| forms.iter().any(|f| u.as_str().contains(f.as_str())));
    named.into_iter().chain(others).collect()
}

/// Body cap by content type. An HTML page is cut at the page cap rather than
/// refused: its hints sit in its head. Anything else may be the image itself.
fn body_cap(content_type: Option<&str>) -> BodyCap {
    match content_type {
        Some(ct) if ct.starts_with("text/html") || ct.starts_with("application/xhtml") => {
            BodyCap::Truncate(MAX_PAGE_BYTES)
        }
        _ => BodyCap::Refuse(COVER_MAX_INPUT_BYTES),
    }
}

/// Largest image, in pixels, the app decodes from the web. Mirrors
/// `CoverCameraHelper.webMaxSourcePixels` on the app side, which checks the
/// declared size of every format; this side adds what the app cannot check.
const WEB_MAX_SOURCE_PIXELS: u64 = 12_000_000;

/// Whether fetched bytes can be handed to the app's cover pipeline: a format
/// it decodes, and for PNG, data that cannot blow up once inflated.
fn check_usable_image(bytes: &[u8]) -> Result<(), WebCoverError> {
    match sniff_image(bytes) {
        Some("png") => check_png(bytes),
        Some(_) => Ok(()),
        None => Err(WebCoverError::UnsupportedImage),
    }
}

/// Refuses a PNG whose image data inflates to more than its header allows.
///
/// The app's PNG decoder inflates all the IDAT data in one go, whatever size
/// the header declares: a few kilobytes of compressed zeros can expand to
/// gigabytes and get the app killed. Here the data is inflated in small
/// pieces, at constant memory, and counted against the raw size the header
/// implies. An animated PNG is refused outright, like an animated WebP: a
/// cover is a still image. Data that is merely corrupt is let through, for
/// the app's decoder to report as unreadable.
fn check_png(bytes: &[u8]) -> Result<(), WebCoverError> {
    use std::io::Read;

    let mut header: Option<(u64, u64, u64, u8)> = None;
    let mut idat = Vec::new();
    let mut pos = 8;
    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let start = pos + 8;
        let Some(end) = start.checked_add(len).filter(|end| *end <= bytes.len()) else {
            break;
        };
        let data = &bytes[start..end];
        match kind {
            b"IHDR" if data.len() >= 13 => {
                let width = u32::from_be_bytes(data[0..4].try_into().unwrap()) as u64;
                let height = u32::from_be_bytes(data[4..8].try_into().unwrap()) as u64;
                let channels = match data[9] {
                    0 | 3 => 1,
                    4 => 2,
                    2 => 3,
                    6 => 4,
                    _ => return Err(WebCoverError::UnsupportedImage),
                };
                header = Some((width, height, channels * u64::from(data[8]), data[12]));
            }
            b"acTL" => return Err(WebCoverError::UnsupportedImage),
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        pos = end + 4; // skip the CRC
    }
    let Some((width, height, bits_per_pixel, interlace)) = header else {
        return Err(WebCoverError::UnsupportedImage);
    };
    if width.saturating_mul(height) > WEB_MAX_SOURCE_PIXELS {
        return Err(PublicFetchError::TooLarge.into());
    }
    // One filter byte per row; interlacing splits the image into seven passes
    // with their own rows, so it gets a generous margin.
    let raw = height * ((width * bits_per_pixel).div_ceil(8) + 1);
    let limit = if interlace == 0 { raw } else { raw * 2 } + 1024;

    let mut inflater = flate2::read::ZlibDecoder::new(idat.as_slice());
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        match inflater.read(&mut buf) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(n) => {
                total += n as u64;
                if total > limit {
                    return Err(PublicFetchError::TooLarge.into());
                }
            }
        }
    }
}

/// The image format of `bytes`, from its magic number, among the formats the
/// app's cover pipeline accepts from the web. An animated WebP is not one of
/// them: its frame sizes are out of reach of a header read.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        // Extended format: a VP8X chunk whose flags byte carries the
        // animation bit (0x02).
        let animated = bytes.len() > 20 && &bytes[12..16] == b"VP8X" && bytes[20] & 0x02 != 0;
        (!animated).then_some("webp")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else {
        None
    }
}

static JSON_LD: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"script[type="application/ld+json"]"#).unwrap());
static META_IMAGE: LazyLock<Vec<Selector>> = LazyLock::new(|| {
    [
        r#"meta[property="og:image:secure_url"]"#,
        r#"meta[property="og:image"]"#,
        r#"meta[name="og:image"]"#,
        r#"meta[name="twitter:image"]"#,
        r#"meta[property="twitter:image"]"#,
        r#"meta[name="twitter:image:src"]"#,
    ]
    .iter()
    .map(|s| Selector::parse(s).unwrap())
    .collect()
});
static BASE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("base[href]").unwrap());
static IMAGE_SRC: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"link[rel~="image_src"]"#).unwrap());
static IMG: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());

/// Every cover image a page declares, most specific first, as absolute http(s)
/// URLs resolved against `page_url`. Duplicates are kept out.
pub fn extract_cover_candidates(html: &str, page_url: &Url) -> Vec<Url> {
    let doc = Html::parse_document(html);
    let mut raw: Vec<String> = Vec::new();

    let mut json_ld = JsonLdImages::default();
    for script in doc.select(&JSON_LD) {
        let text: String = script.text().collect();
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            json_ld.collect(&value, 0);
        }
    }
    raw.append(&mut json_ld.books);
    raw.append(&mut json_ld.identified_products);
    for selector in META_IMAGE.iter() {
        raw.extend(
            doc.select(selector)
                .filter_map(|el| el.value().attr("content"))
                .map(str::to_owned),
        );
    }
    raw.extend(
        doc.select(&IMAGE_SRC)
            .filter_map(|el| el.value().attr("href"))
            .map(str::to_owned),
    );
    raw.append(&mut json_ld.other_products);
    // Content images before the site's own (logo, menu, footer badges),
    // which a page without hints often lists first.
    let (chrome, content): (Vec<_>, Vec<_>) = doc
        .select(&IMG)
        .filter_map(|el| img_source(el.value()).map(|src| (is_site_chrome(&el), src)))
        .partition(|(chrome, _)| *chrome);
    raw.extend(content.into_iter().map(|(_, src)| src));
    raw.extend(chrome.into_iter().map(|(_, src)| src));

    // Relative links resolve against `<base href>` when the page sets one.
    let base = doc
        .select(&BASE)
        .next()
        .and_then(|el| el.value().attr("href"))
        .and_then(|href| page_url.join(href.trim()).ok())
        .unwrap_or_else(|| page_url.clone());

    let mut out: Vec<Url> = Vec::new();
    for candidate in raw {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        let Ok(url) = base.join(candidate) else {
            continue;
        };
        if matches!(url.scheme(), "http" | "https") && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// The address an `<img>` really shows: lazy-loading sites put a placeholder
/// in `src` and the image in a `data-*` attribute. `None` for an image whose
/// declared size says it is a pixel, an icon or a logo.
fn img_source(img: &scraper::node::Element) -> Option<String> {
    let too_small = ["width", "height"].iter().any(|side| {
        img.attr(side)
            .and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok())
            .is_some_and(|v| v < MIN_IMG_SIDE)
    });
    if too_small {
        return None;
    }
    ["data-src", "data-original", "data-lazy-src", "src"]
        .iter()
        .find_map(|attr| img.attr(attr).filter(|v| !v.trim().is_empty()))
        .map(str::to_owned)
}

/// Words that mark an image as part of the site rather than of the page's
/// content, in its id or class. Not in its alt text or file name: those
/// carry the book's title, and a book can be called "No Logo".
const CHROME_WORDS: [&str; 6] = ["logo", "icon", "avatar", "banner", "sprite", "favicon"];

/// Whether an `<img>` looks like part of the site (logo, menu, footer
/// badge) rather than the book shown: inside the site's `<header>`, `<nav>`
/// or `<footer>` (an `<article>` or `<main>` has headers of its own, which
/// hold content), or with an id or class that says so.
fn is_site_chrome(img: &scraper::ElementRef) -> bool {
    let names: Vec<&str> = img
        .ancestors()
        .filter_map(|node| node.value().as_element().map(|e| e.name()))
        .collect();
    let in_content = names.iter().any(|n| matches!(*n, "article" | "main"));
    let in_chrome = names
        .iter()
        .any(|n| matches!(*n, "header" | "nav" | "footer"));
    if in_chrome && !in_content {
        return true;
    }
    let element = img.value();
    [element.attr("id"), element.attr("class")]
        .into_iter()
        .flatten()
        .any(|text| {
            text.split(|c: char| !c.is_alphanumeric()).any(|token| {
                let token = token.to_lowercase();
                let singular = token.strip_suffix('s').unwrap_or(&token);
                CHROME_WORDS.contains(&singular)
            })
        })
}

/// The images a page's JSON-LD declares for what it sells or describes,
/// sorted by how surely they are the book's cover.
#[derive(Default)]
struct JsonLdImages {
    /// A schema.org `Book`.
    books: Vec<String>,
    /// A `Product` with an ISBN or GTIN: a bookshop's book page.
    identified_products: Vec<String>,
    /// Any other `Product`, which may be anything sold on the page.
    other_products: Vec<String>,
}

impl JsonLdImages {
    /// Walk a JSON-LD value, `@graph` arrays and nested objects included.
    fn collect(&mut self, value: &Value, depth: usize) {
        if depth > MAX_JSON_LD_DEPTH {
            return;
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    self.collect(item, depth + 1);
                }
            }
            Value::Object(map) => {
                if let (Some(t), Some(image)) = (map.get("@type"), map.get("image")) {
                    if has_type(t, "Book") {
                        collect_image_urls(image, &mut self.books);
                    } else if has_type(t, "Product") {
                        let identified = ["isbn", "gtin13", "gtin"]
                            .iter()
                            .any(|k| map.get(*k).is_some_and(|v| !v.is_null()));
                        let target = if identified {
                            &mut self.identified_products
                        } else {
                            &mut self.other_products
                        };
                        collect_image_urls(image, target);
                    }
                }
                for (key, child) in map {
                    if key != "image" {
                        self.collect(child, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether a JSON-LD `@type` (a name, a URL or an array of them) is `name`.
fn has_type(t: &Value, name: &str) -> bool {
    let matches = |s: &str| s.rsplit(['/', ':']).next().unwrap_or(s) == name;
    match t {
        Value::String(s) => matches(s),
        Value::Array(items) => items.iter().any(|i| i.as_str().is_some_and(matches)),
        _ => false,
    }
}

/// A schema.org `image` is a URL string, an `ImageObject` (`url` or
/// `contentUrl`), or an array of either.
fn collect_image_urls(image: &Value, out: &mut Vec<String>) {
    match image {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_image_urls(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["contentUrl", "url"] {
                if let Some(Value::String(s)) = map.get(key) {
                    out.push(s.clone());
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];

    fn page() -> Url {
        Url::parse("https://www.editions-exemple.fr/catalogue/livre/la-fin-d-un-monde").unwrap()
    }

    fn first(html: &str) -> Option<String> {
        extract_cover_candidates(html, &page())
            .into_iter()
            .next()
            .map(|u| u.to_string())
    }

    #[test]
    fn reads_og_image() {
        let html = r#"<html><head>
            <meta property="og:title" content="La fin d'un monde">
            <meta property="og:image" content="https://cdn.editions-exemple.fr/covers/9782234086494.jpg">
            </head><body></body></html>"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://cdn.editions-exemple.fr/covers/9782234086494.jpg")
        );
    }

    #[test]
    fn resolves_relative_links_against_the_page() {
        let html = r#"<meta property="og:image" content="/media/couv/42.jpg">"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://www.editions-exemple.fr/media/couv/42.jpg")
        );
        let html = r#"<meta property="og:image" content="//img.editions-exemple.fr/42.jpg">"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://img.editions-exemple.fr/42.jpg")
        );
    }

    #[test]
    fn resolves_relative_links_against_the_base_element() {
        let html = r#"<head><base href="https://static.example.org/site/">
            <meta property="og:image" content="img/couv.jpg"></head>"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://static.example.org/site/img/couv.jpg")
        );
    }

    #[test]
    fn decodes_html_entities_in_attributes() {
        let html =
            r#"<meta property="og:image" content="https://cdn.example.org/img?id=42&amp;w=600">"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://cdn.example.org/img?id=42&w=600")
        );
    }

    #[test]
    fn a_json_ld_book_image_wins_over_og_image() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://www.editions-exemple.fr/logo.png">
            <script type="application/ld+json">
              {"@context":"https://schema.org","@type":"Book","name":"X",
               "isbn":"9782234086494","image":"https://cdn.editions-exemple.fr/9782234086494.jpg"}
            </script></head></html>"#;
        let all = extract_cover_candidates(html, &page());
        assert_eq!(
            all.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                "https://cdn.editions-exemple.fr/9782234086494.jpg",
                "https://www.editions-exemple.fr/logo.png",
            ]
        );
    }

    #[test]
    fn reads_json_ld_in_graphs_arrays_and_image_objects() {
        let html = r#"<script type="application/ld+json">
            {"@context":"https://schema.org","@graph":[
              {"@type":"WebPage","image":"https://example.org/not-the-book.jpg"},
              {"@type":["Book","Product"],
               "image":{"@type":"ImageObject","contentUrl":"https://example.org/cover.jpg"}}
            ]}</script>
            <script type="application/ld+json">
            [{"@type":"http://schema.org/Book","image":["https://example.org/c1.jpg","https://example.org/c2.jpg"]}]
            </script>"#;
        let all: Vec<String> = extract_cover_candidates(html, &page())
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            all,
            vec![
                "https://example.org/cover.jpg",
                "https://example.org/c1.jpg",
                "https://example.org/c2.jpg",
            ],
            "only Book images, never the WebPage one"
        );
    }

    #[test]
    fn a_bookshop_product_with_an_isbn_wins_over_a_generic_og_image() {
        // Shape of a real bookshop page: the og:image is the site's generic
        // banner, the book is a schema.org Product carrying its ISBN.
        let html = r#"<html><head>
            <meta property="og:image" content="https://shop.example.org/img/og-default.jpg">
            <script type="application/ld+json">
              {"@context":"https://schema.org","@graph":[{"@type":"Organization",
               "logo":"https://shop.example.org/img/logo.png"}]}
            </script>
            <script type="application/ld+json">
              {"@context":"https://schema.org","@type":"Product","name":"Le Vendeur de Saris",
               "image":"https://images.example.net/9782290003299-475x500-1.jpg",
               "gtin13":"9782290003299","isbn":"9782290003299"}
            </script></head></html>"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://images.example.net/9782290003299-475x500-1.jpg")
        );
    }

    #[test]
    fn a_product_without_identifier_comes_after_the_page_hints() {
        // Without an ISBN or GTIN, a Product may be anything sold on the
        // page (a tote bag in a sidebar): the page's own hints come first.
        let html = r#"<meta property="og:image" content="https://example.org/og.jpg">
            <script type="application/ld+json">
              {"@type":"Product","image":"https://example.org/tote-bag.jpg"}
            </script>"#;
        let all: Vec<String> = extract_cover_candidates(html, &page())
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            all,
            vec![
                "https://example.org/og.jpg",
                "https://example.org/tote-bag.jpg"
            ]
        );
    }

    #[test]
    fn a_book_still_wins_over_a_product() {
        let html = r#"<script type="application/ld+json">
              [{"@type":"Product","isbn":"9782290003299","image":"https://example.org/p.jpg"},
               {"@type":"Book","image":"https://example.org/b.jpg"}]
            </script>"#;
        assert_eq!(first(html).as_deref(), Some("https://example.org/b.jpg"));
    }

    #[test]
    fn ignores_broken_json_ld_and_falls_back() {
        let html = r#"<script type="application/ld+json">{ not json </script>
            <meta name="twitter:image" content="https://example.org/tw.jpg">"#;
        assert_eq!(first(html).as_deref(), Some("https://example.org/tw.jpg"));
    }

    #[test]
    fn falls_back_to_image_src() {
        let html = r#"<link rel="image_src" href="https://example.org/src.jpg">"#;
        assert_eq!(first(html).as_deref(), Some("https://example.org/src.jpg"));
    }

    #[test]
    fn drops_non_http_candidates_and_duplicates() {
        let html = r#"<meta property="og:image" content="javascript:alert(1)">
            <meta property="og:image" content="data:image/png;base64,AAAA">
            <meta property="og:image" content="https://example.org/a.jpg">
            <meta name="twitter:image" content="https://example.org/a.jpg">"#;
        let all = extract_cover_candidates(html, &page());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].as_str(), "https://example.org/a.jpg");
    }

    #[test]
    fn a_page_without_any_image_has_no_candidate() {
        assert!(
            extract_cover_candidates("<html><body><p>Catalogue</p></body></html>", &page())
                .is_empty()
        );
        assert!(extract_cover_candidates("", &page()).is_empty());
    }

    #[test]
    fn sniffs_supported_image_formats() {
        assert_eq!(sniff_image(JPEG), Some("jpeg"));
        assert_eq!(
            sniff_image(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]),
            Some("png")
        );
        assert_eq!(sniff_image(b"RIFF\x10\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_image(b"GIF89a...."), Some("gif"));
        assert_eq!(
            sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            None
        );
        assert_eq!(sniff_image(b"<!doctype html>"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn an_animated_webp_is_not_a_usable_image() {
        // The app's pipeline refuses an animated WebP from the web (its frame
        // sizes are out of reach of a header read); calling it usable here
        // would stop the search on it instead of trying the next candidate.
        let vp8x = |flags: u8| {
            let mut b = b"RIFF\x1e\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00".to_vec();
            b.extend_from_slice(&[flags, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            b
        };
        assert_eq!(sniff_image(&vp8x(0x02)), None, "animation flag");
        assert_eq!(sniff_image(&vp8x(0x12)), None, "animation with alpha");
        assert_eq!(sniff_image(&vp8x(0x10)), Some("webp"), "alpha only");
    }

    #[tokio::test]
    async fn skips_an_animated_webp_candidate() {
        let server = MockServer::start().await;
        let animated = {
            let mut b = b"RIFF\x1e\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00".to_vec();
            b.extend_from_slice(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            b
        };
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(format!(
                r#"<meta property="og:image" content="{0}/anim.webp">
                   <meta name="twitter:image" content="{0}/couv.jpg">"#,
                server.uri()
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/anim.webp"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(animated, "image/webp"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/couv.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;

        let cover = fetch_test(format!("{}/livre", server.uri())).await.unwrap();
        assert_eq!(cover.image_url, format!("{}/couv.jpg", server.uri()));
    }

    /// A PNG with the given header, whose IDAT is `pixels` zero bytes
    /// compressed (a flat image compresses to almost nothing).
    fn png(width: u32, height: u32, inflated: usize, animated: bool) -> Vec<u8> {
        use std::io::Write;
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&[0, 0, 0, 0]); // CRC, not checked here
        }
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, not interlaced
        chunk(&mut out, b"IHDR", &ihdr);
        if animated {
            chunk(&mut out, b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        z.write_all(&vec![0u8; inflated]).unwrap();
        chunk(&mut out, b"IDAT", &z.finish().unwrap());
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn a_png_whose_data_inflates_past_its_size_is_refused() {
        // 100x100 RGBA holds 100 * (1 + 400) bytes once inflated; this one
        // expands to 50 MB from a few dozen kilobytes.
        let bomb = png(100, 100, 50 * 1024 * 1024, false);
        assert!(bomb.len() < 200 * 1024, "the bomb is small: {}", bomb.len());
        assert_eq!(
            check_png(&bomb),
            Err(WebCoverError::Fetch(PublicFetchError::TooLarge))
        );
    }

    #[test]
    fn an_honest_png_passes() {
        assert_eq!(check_png(&png(100, 100, 100 * 401, false)), Ok(()));
    }

    #[test]
    fn a_png_declaring_too_many_pixels_or_animated_is_refused() {
        assert_eq!(
            check_png(&png(6000, 6000, 10, false)),
            Err(WebCoverError::Fetch(PublicFetchError::TooLarge))
        );
        assert_eq!(
            check_png(&png(100, 100, 100 * 401, true)),
            Err(WebCoverError::UnsupportedImage)
        );
    }

    #[test]
    fn img_tags_come_last_lazy_sources_first_tracking_pixels_out() {
        let html = r#"<meta property="og:image" content="https://example.org/og.jpg">
            <img src="https://example.org/pixel.gif" width="1" height="1">
            <img src="/placeholder.gif" data-src="/covers/real.jpg" alt="cover">
            <img src="https://example.org/plain.jpg">
            <img src="data:image/gif;base64,R0lGOD">"#;
        let all: Vec<String> = extract_cover_candidates(html, &page())
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            all,
            vec![
                "https://example.org/og.jpg",
                "https://www.editions-exemple.fr/covers/real.jpg",
                "https://example.org/plain.jpg",
            ]
        );
    }

    #[test]
    fn site_chrome_images_go_after_the_content_ones() {
        // Shape of a real publisher page that declares nothing: the site logo
        // comes first in the markup, the cover second.
        let html = r#"<html><body>
            <header><a href="/"><img src="https://example.org/brand.png" alt="Shop"></a></header>
            <img src="http://example.org/wp-content/uploads/almasta_logo.png" alt="Editions" id="logo">
            <nav><img src="https://example.org/menu.png"></nav>
            <main>
              <img src="https://example.org/icons/cart.svg" class="cart-icon">
              <img src="https://example.org/uploads/Slow_Sex_femme_web_def.jpg" alt="">
            </main>
            <footer><img src="https://example.org/partner.png"></footer>
            </body></html>"#;
        let all: Vec<String> = extract_cover_candidates(html, &page())
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            all[0],
            "https://example.org/uploads/Slow_Sex_femme_web_def.jpg"
        );
        assert_eq!(
            all.len(),
            6,
            "demoted, never dropped: the user can still step to them"
        );
    }

    #[test]
    fn a_lazy_loaded_logo_is_recognised_by_its_class() {
        // The src is a placeholder pixel and the file name proves nothing;
        // the class says what the image is for.
        let html = r#"<img src="/blank.gif" data-src="https://example.org/brand.png" class="site-logo">
            <img src="https://example.org/couv/livre.jpg">"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://example.org/couv/livre.jpg")
        );
    }

    #[test]
    fn an_article_header_and_a_title_in_alt_are_content() {
        // WordPress wraps a post's cover in <article><header>; a book called
        // "No Logo" says so in its alt text. Neither is the site's chrome.
        let html = r#"<body>
            <article><header class="entry-header">
              <img src="https://example.org/uploads/no-logo-cover.jpg" alt="No Logo, Naomi Klein">
            </header></article>
            <img src="https://example.org/uploads/author.jpg">
            </body>"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://example.org/uploads/no-logo-cover.jpg")
        );
    }

    #[test]
    fn a_chrome_word_must_be_a_whole_word() {
        // "Iconoclaste" is a publisher, not an icon; "logos" still counts.
        let html = r#"<img src="https://example.org/a/brands.png" class="partner-logos">
            <img src="https://example.org/couv/ma-reine.jpg" alt="Iconoclaste, Ma reine">"#;
        assert_eq!(
            first(html).as_deref(),
            Some("https://example.org/couv/ma-reine.jpg")
        );
    }

    #[test]
    fn images_named_after_the_isbn_come_first() {
        let urls: Vec<Url> = [
            "https://shop.example.org/img/og-default.jpg",
            "https://cdn.example.net/0306406152-cover.jpg",
            "https://cdn.example.net/9782290003299-475x500-1.jpg",
            "https://cdn.example.net/other.jpg",
        ]
        .iter()
        .map(|u| Url::parse(u).unwrap())
        .collect();

        let ranked = rank_by_isbn(urls.clone(), Some("978-2-290-00329-9"));
        assert_eq!(
            ranked[0].as_str(),
            "https://cdn.example.net/9782290003299-475x500-1.jpg"
        );
        assert_eq!(
            &ranked[1..],
            &[urls[0].clone(), urls[1].clone(), urls[3].clone()]
        );

        // The ISBN-10 form counts too.
        let ranked = rank_by_isbn(urls.clone(), Some("9780306406157"));
        assert_eq!(
            ranked[0].as_str(),
            "https://cdn.example.net/0306406152-cover.jpg"
        );

        // No ISBN, or one that does not parse: the page order stands.
        assert_eq!(rank_by_isbn(urls.clone(), None), urls);
        assert_eq!(rank_by_isbn(urls.clone(), Some("not-an-isbn")), urls);
    }

    #[tokio::test]
    async fn the_isbn_picks_the_book_image_over_the_page_hints() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(format!(
                r#"<meta property="og:image" content="{0}/og-default.jpg">
                   <img src="{0}/9782290003299-475x500-1.jpg">"#,
                server.uri()
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;

        let url = Url::parse(&format!("{}/livre", server.uri())).unwrap();
        let cover = fetch_cover_with_policy(
            url,
            FetchPolicy::PublicOrLoopbackForTests,
            PICK_DEADLINE,
            Some("9782290003299".to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(
            cover.image_url,
            format!("{}/9782290003299-475x500-1.jpg", server.uri())
        );
        assert_eq!(
            cover.candidates,
            vec![
                format!("{}/9782290003299-475x500-1.jpg", server.uri()),
                format!("{}/og-default.jpg", server.uri()),
            ],
            "every candidate is handed back for the preview to step through"
        );
    }

    #[tokio::test]
    async fn hands_back_a_bounded_list_of_candidates() {
        let server = MockServer::start().await;
        let imgs: String = (0..30)
            .map(|i| format!(r#"<img src="{}/c{i}.jpg">"#, server.uri()))
            .collect();
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(imgs))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;

        let cover = fetch_test(format!("{}/livre", server.uri())).await.unwrap();
        assert_eq!(cover.candidates.len(), MAX_LISTED_CANDIDATES);
    }

    #[tokio::test]
    async fn a_pasted_image_is_its_own_only_candidate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;
        let url = format!("{}/couv.jpg", server.uri());
        let cover = fetch_test(url.clone()).await.unwrap();
        assert_eq!(cover.candidates, vec![url]);
    }

    #[tokio::test]
    async fn fetches_one_chosen_candidate() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/couv.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(html("<html></html>".to_owned()))
            .mount(&server)
            .await;

        let image = |p: &str| Url::parse(&format!("{}{p}", server.uri())).unwrap();
        let cover =
            fetch_image_with_policy(image("/couv.jpg"), FetchPolicy::PublicOrLoopbackForTests)
                .await
                .unwrap();
        assert_eq!(cover.bytes, JPEG);
        assert!(cover.candidates.is_empty());

        // A candidate is an image or nothing: never parsed as a page.
        let err = fetch_image_with_policy(image("/page"), FetchPolicy::PublicOrLoopbackForTests)
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::UnsupportedImage);

        // The address comes back from the app: it is checked again.
        let err = fetch_cover_image("https://192.168.1.10/couv.jpg")
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::Fetch(PublicFetchError::BlockedAddress));
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(WebCoverError::NoImageFound.code(), "no_image_found");
        assert_eq!(
            WebCoverError::Fetch(PublicFetchError::HttpStatus(503)).code(),
            "http_status"
        );
    }

    async fn fetch_test(url: String) -> Result<WebCover, WebCoverError> {
        fetch_cover_with_policy(
            Url::parse(&url).unwrap(),
            FetchPolicy::PublicOrLoopbackForTests,
            PICK_DEADLINE,
            None,
        )
        .await
    }

    fn html(body: String) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/html; charset=utf-8")
    }

    #[tokio::test]
    async fn fetches_the_image_a_page_declares() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/livre/42"))
            .respond_with(html(
                r#"<meta property="og:image" content="/couv/42.jpg">"#.to_owned(),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/couv/42.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;

        let cover = fetch_test(format!("{}/livre/42", server.uri()))
            .await
            .unwrap();
        assert_eq!(cover.bytes, JPEG);
        assert_eq!(cover.image_url, format!("{}/couv/42.jpg", server.uri()));
    }

    #[tokio::test]
    async fn accepts_a_direct_image_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/couv.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;
        let url = format!("{}/couv.jpg", server.uri());
        let cover = fetch_test(url.clone()).await.unwrap();
        assert_eq!(cover.bytes, JPEG);
        assert_eq!(cover.image_url, url);
    }

    #[tokio::test]
    async fn parses_only_the_head_of_a_long_page() {
        // Without a Content-Type the body may be an image, so it is read up to
        // the image cap; once it turns out not to be one, only its first
        // MAX_PAGE_BYTES are parsed as a page.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_PAGE_BYTES + 1]))
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/livre", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::NoImageFound);
    }

    #[tokio::test]
    async fn reads_the_hints_of_a_page_longer_than_the_cap() {
        // A product page can exceed the cap in raw HTML; its hints are in the
        // head, so the page is cut rather than refused.
        let server = MockServer::start().await;
        let mut page = format!(
            r#"<html><head><meta property="og:image" content="{}/couv.jpg"></head><body>"#,
            server.uri()
        );
        page.push_str(&"x".repeat(MAX_PAGE_BYTES + 10));
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(page))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/couv.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;
        let cover = fetch_test(format!("{}/livre", server.uri())).await.unwrap();
        assert_eq!(cover.image_url, format!("{}/couv.jpg", server.uri()));
    }

    #[tokio::test]
    async fn falls_back_to_the_next_candidate_when_one_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(format!(
                r#"<script type="application/ld+json">
                   {{"@type":"Book","image":"{0}/gone.jpg"}}</script>
                   <meta property="og:image" content="{0}/logo.svg">
                   <meta name="twitter:image" content="{0}/couv.jpg">"#,
                server.uri()
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone.jpg"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/logo.svg"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"<svg/>".to_vec(), "image/svg+xml"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/couv.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;

        let cover = fetch_test(format!("{}/livre", server.uri())).await.unwrap();
        assert_eq!(cover.image_url, format!("{}/couv.jpg", server.uri()));
    }

    #[tokio::test]
    async fn tries_every_listed_candidate_and_no_more() {
        // The cover may well be the fourth declared image: every candidate
        // kept is tried, but a page declaring more cannot make us fire a
        // request per tag.
        let server = MockServer::start().await;
        let metas: String = (0..MAX_LISTED_CANDIDATES + 5)
            .map(|i| {
                format!(
                    r#"<meta property="og:image" content="{}/c{i}.jpg">"#,
                    server.uri()
                )
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(metas))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex(r"^/c\d+\.jpg$"))
            .respond_with(ResponseTemplate::new(404))
            .expect(MAX_LISTED_CANDIDATES as u64)
            .mount(&server)
            .await;

        let err = fetch_test(format!("{}/livre", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::Fetch(PublicFetchError::HttpStatus(404)));
    }

    #[tokio::test]
    async fn gives_up_at_the_overall_deadline() {
        // Each request stays under its own timeouts, but the dialog waiting on
        // the whole pick cannot be dismissed: the pick as a whole is bounded.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(JPEG.to_vec(), "image/jpeg")
                    .set_delay(std::time::Duration::from_secs(2)),
            )
            .mount(&server)
            .await;
        let err = fetch_cover_with_policy(
            Url::parse(&format!("{}/couv.jpg", server.uri())).unwrap(),
            FetchPolicy::PublicOrLoopbackForTests,
            std::time::Duration::from_millis(200),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err, WebCoverError::Fetch(PublicFetchError::Network));
    }

    #[tokio::test]
    async fn reports_a_page_without_image() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(html("<html><title>Catalogue</title></html>".to_owned()))
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/livre", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::NoImageFound);
    }

    #[tokio::test]
    async fn refuses_an_image_the_pipeline_cannot_decode() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(
                r#"<meta property="og:image" content="/couv.svg">"#.to_owned(),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/couv.svg"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"<svg/>".to_vec(), "image/svg+xml"),
            )
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/livre", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::UnsupportedImage);

        let err = fetch_test(format!("{}/couv.svg", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::UnsupportedImage, "pasted directly too");
    }

    #[tokio::test]
    async fn skips_a_declared_image_on_a_private_address() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/livre"))
            .respond_with(html(format!(
                r#"<meta property="og:image" content="https://192.168.1.10/couv.jpg">
                   <meta name="twitter:image" content="{}/couv.jpg">"#,
                server.uri()
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/couv.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(JPEG.to_vec(), "image/jpeg"))
            .mount(&server)
            .await;
        let cover = fetch_test(format!("{}/livre", server.uri())).await.unwrap();
        assert_eq!(cover.image_url, format!("{}/couv.jpg", server.uri()));
    }
}
