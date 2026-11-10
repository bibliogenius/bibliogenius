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
//! 4. the `image` of any other `Product`.
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
use crate::utils::public_fetch::{self, FetchPolicy, PublicFetchError};

/// Cap on an HTML page. A publisher's book page is far below this; the cap
/// only bounds what a hostile or broken server can make us hold in memory.
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// Declared images tried, in order, before giving up: the first one may be
/// gone (404) or a format we cannot use while the next one is the cover.
/// Bounded so a page cannot make us fire a request per tag.
pub const MAX_IMAGE_CANDIDATES: usize = 3;

/// JSON-LD nesting walked before giving up on a block.
const MAX_JSON_LD_DEPTH: usize = 8;

/// A cover found on the web: the raw image and the address it was found at.
#[derive(Debug)]
pub struct WebCover {
    pub bytes: Vec<u8>,
    /// The image URL as the page declared it (or as the user pasted it), not
    /// the post-redirect one: a CDN's redirect target is often short-lived.
    pub image_url: String,
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
/// page or the image itself.
pub async fn fetch_cover_from_page(raw_url: &str) -> Result<WebCover, WebCoverError> {
    let url = public_fetch::parse_public_https_url(raw_url)?;
    fetch_cover_with_policy(url, FetchPolicy::PublicInternet, PICK_DEADLINE).await
}

/// Ceiling on a whole pick (page, redirects, up to [`MAX_IMAGE_CANDIDATES`]
/// images). The app's dialog cannot be dismissed while it runs, so a slow or
/// hostile server must not hold it longer than this. Leaves room for a 10 MB
/// image over a 1 Mbit/s link.
pub const PICK_DEADLINE: Duration = Duration::from_secs(120);

pub(crate) async fn fetch_cover_with_policy(
    url: Url,
    policy: FetchPolicy,
    deadline: Duration,
) -> Result<WebCover, WebCoverError> {
    tokio::time::timeout(deadline, pick_cover(url, policy))
        .await
        .unwrap_or(Err(PublicFetchError::Network.into()))
}

async fn pick_cover(url: Url, policy: FetchPolicy) -> Result<WebCover, WebCoverError> {
    let page = public_fetch::fetch_with_policy(url.clone(), &body_cap, policy).await?;
    if sniff_image(&page.bytes).is_some() {
        return Ok(WebCover {
            bytes: page.bytes,
            image_url: url.to_string(),
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
    // An untyped body was read up to the image cap; as a page it gets the
    // page cap.
    if page.bytes.len() > MAX_PAGE_BYTES {
        return Err(PublicFetchError::TooLarge.into());
    }

    // Parsing a page of up to MAX_PAGE_BYTES is CPU work: keep it off the
    // async runtime's worker threads.
    let html = String::from_utf8_lossy(&page.bytes).into_owned();
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
        .take(MAX_IMAGE_CANDIDATES)
        .collect();

    // The last failure is reported when none works.
    let mut error = WebCoverError::NoImageFound;
    for image_url in usable {
        match public_fetch::fetch_with_policy(image_url.clone(), &|_| COVER_MAX_INPUT_BYTES, policy)
            .await
        {
            Ok(image) if sniff_image(&image.bytes).is_some() => {
                return Ok(WebCover {
                    bytes: image.bytes,
                    image_url: image_url.to_string(),
                });
            }
            Ok(_) => error = WebCoverError::UnsupportedImage,
            Err(e) => error = e.into(),
        }
    }
    Err(error)
}

/// Body cap by content type: HTML pages get the page cap, anything else may be
/// the image itself.
fn body_cap(content_type: Option<&str>) -> usize {
    match content_type {
        Some(ct) if ct.starts_with("text/html") || ct.starts_with("application/xhtml") => {
            MAX_PAGE_BYTES
        }
        _ => COVER_MAX_INPUT_BYTES,
    }
}

/// Many older publisher sites still declare `http://` image links while
/// serving the same path over https. Try the https form rather than dropping
/// the candidate; anything else than http(s) is dropped.
fn upgrade_to_https(mut url: Url) -> Option<Url> {
    match url.scheme() {
        "https" => Some(url),
        "http" => {
            url.set_scheme("https").ok()?;
            // An explicit :80 would not survive the scheme change sensibly.
            if url.port().is_some() {
                return None;
            }
            Some(url)
        }
        _ => None,
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
static IMAGE_SRC: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"link[rel~="image_src"]"#).unwrap());

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

    let mut out: Vec<Url> = Vec::new();
    for candidate in raw {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        let Ok(url) = page_url.join(candidate) else {
            continue;
        };
        if matches!(url.scheme(), "http" | "https") && !out.contains(&url) {
            out.push(url);
        }
    }
    out
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
    fn a_page_without_hints_has_no_candidate() {
        assert!(
            extract_cover_candidates("<html><body><img src=x.jpg></body></html>", &page())
                .is_empty()
        );
        assert!(extract_cover_candidates("", &page()).is_empty());
    }

    #[test]
    fn upgrades_http_links_to_https() {
        let up = upgrade_to_https(Url::parse("http://example.org/a.jpg").unwrap()).unwrap();
        assert_eq!(up.as_str(), "https://example.org/a.jpg");
        assert!(upgrade_to_https(Url::parse("http://example.org:8080/a.jpg").unwrap()).is_none());
        assert!(upgrade_to_https(Url::parse("ftp://example.org/a.jpg").unwrap()).is_none());
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
    async fn holds_an_untyped_page_to_the_page_cap() {
        // Without a Content-Type the body may be an image, so it is read up to
        // the image cap; once it turns out not to be one, the page cap applies.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; MAX_PAGE_BYTES + 1]))
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/livre", server.uri()))
            .await
            .unwrap_err();
        assert_eq!(err, WebCoverError::Fetch(PublicFetchError::TooLarge));
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
    async fn tries_a_bounded_number_of_candidates() {
        let server = MockServer::start().await;
        let metas: String = (0..6)
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
            .and(wiremock::matchers::path_regex(r"^/c\d\.jpg$"))
            .respond_with(ResponseTemplate::new(404))
            .expect(MAX_IMAGE_CANDIDATES as u64)
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
