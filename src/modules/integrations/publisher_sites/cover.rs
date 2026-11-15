//! The cover on a book's page, once [`super::lookup::find_book_page`] found
//! it: the page's images in turn, under the rules every fetch of this module
//! follows (robots.txt for images as for pages, the pause between two
//! requests to a host). `publisher_sites measure` takes its covers through
//! [`page_cover`], and so will the app: what the tool measures is what the
//! app gets.

use url::Url;

use super::fetch::{FetchError, Page, SiteFetcher};
use super::lookup::{cover_images, off_runtime};
use crate::modules::integrations::web_cover::{
    MAX_LISTED_CANDIDATES, WebCover, check_usable_image_blocking,
};
use crate::utils::public_fetch::upgrade_to_https;

/// Why no cover came out of the book's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageCoverError {
    /// The site does not let us have the cover: robots.txt forbids it, a
    /// robots.txt the server fails to serve forbids everything, or the site
    /// refuses us.
    Forbidden,
    /// The image's host does not answer: the cover may be there.
    Unreachable,
    /// The page shows no image of the book.
    NoImage,
    /// The page's images are gone or in a format the app cannot use.
    Broken,
}

/// The cover on the book's `page`: its images in the order of
/// [`cover_images`], the first that loads and is an image the app can use.
///
/// The first image the site does not let us have ends the search: the
/// page's other images then are a button or a banner, not a stand-in for the
/// cover. The candidates listed for the app's "next image" stop there too,
/// so stepping through them never fetches what robots.txt forbids.
pub async fn page_cover(
    fetcher: &dyn SiteFetcher,
    page: &Page,
    isbn: &str,
) -> Result<WebCover, PageCoverError> {
    let (owned, wanted) = (page.clone(), isbn.to_owned());
    let images: Vec<Url> = off_runtime(move || cover_images(&owned, &wanted))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(upgrade_to_https)
        .take(MAX_LISTED_CANDIDATES)
        .collect();
    if images.is_empty() {
        return Err(PageCoverError::NoImage);
    }
    let mut candidates = Vec::new();
    for image in images {
        match fetcher.may_fetch(&image).await {
            Ok(true) => candidates.push(image),
            Ok(false) | Err(FetchError::Blocked(_)) => break,
            Err(_) if candidates.is_empty() => return Err(PageCoverError::Unreachable),
            Err(_) => break,
        }
    }
    if candidates.is_empty() {
        return Err(PageCoverError::Forbidden);
    }
    let listed: Vec<String> = candidates.iter().map(Url::to_string).collect();
    for image in &candidates {
        match fetcher.get_image(image).await {
            Ok(bytes) => {
                if let Ok(bytes) = check_usable_image_blocking(bytes).await {
                    return Ok(WebCover {
                        bytes,
                        image_url: image.to_string(),
                        candidates: listed,
                    });
                }
            }
            // Refused on the way: a redirect to a forbidden address, a 403.
            Err(FetchError::Disallowed | FetchError::Blocked(_)) => {
                return Err(PageCoverError::Forbidden);
            }
            Err(_) => {}
        }
    }
    Err(PageCoverError::Broken)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::integrations::publisher_sites::test_site::FakeSite;

    const ISBN: &str = "9782000000013";
    const BOOK: &str = "https://editions.example/livre/le-grand-voyage-9782000000013";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];

    /// The book's page, the images given, in order, as tags.
    fn page_with(images: &[&str]) -> Page {
        let tags: String = images
            .iter()
            .map(|src| format!(r#"<img src="{src}" width="400" height="600">"#))
            .collect();
        Page {
            url: Url::parse(BOOK).unwrap(),
            html: format!(
                r#"<html><head><script type="application/ld+json">{{"@type":"Book","isbn":"{ISBN}"}}</script></head><body><h1>Le Grand Voyage</h1>{tags}</body></html>"#
            ),
            truncated: false,
        }
    }

    #[tokio::test]
    async fn the_first_usable_image_is_the_cover() {
        let (broken, good) = (
            "https://editions.example/couv/a.jpg",
            "https://editions.example/couv/b.jpg",
        );
        let site = FakeSite::default()
            .image(broken, b"<html>not an image</html>")
            .image(good, JPEG);
        let cover = page_cover(&site, &page_with(&[broken, good]), ISBN)
            .await
            .unwrap();
        assert_eq!(cover.image_url, good);
        assert_eq!(cover.bytes, JPEG);
        assert_eq!(cover.candidates, [broken, good]);
    }

    #[tokio::test]
    async fn an_image_robots_txt_forbids_ends_the_list() {
        let (allowed, forbidden, after) = (
            "https://editions.example/couv/a.jpg",
            "https://editions.example/prive/b.jpg",
            "https://editions.example/couv/c.jpg",
        );
        let site = || {
            FakeSite {
                forbidden: vec!["/prive/".into()],
                ..FakeSite::default()
            }
            .image(allowed, JPEG)
            .image(after, JPEG)
        };
        let cover = page_cover(&site(), &page_with(&[allowed, forbidden, after]), ISBN)
            .await
            .unwrap();
        // The images after a forbidden one are not offered for "next image".
        assert_eq!(cover.candidates, [allowed]);
        // The cover itself forbidden: no other image of the page instead.
        let site = site();
        assert_eq!(
            page_cover(&site, &page_with(&[forbidden, after]), ISBN)
                .await
                .unwrap_err(),
            PageCoverError::Forbidden
        );
        assert!(!site.asked.lock().unwrap().iter().any(|u| u == after));
    }

    #[tokio::test]
    async fn an_http_image_is_fetched_over_https() {
        let site =
            FakeSite::default().image("https://editions.example/couv/9782000000013.jpg", JPEG);
        let page = page_with(&["http://editions.example/couv/9782000000013.jpg"]);
        let cover = page_cover(&site, &page, ISBN).await.unwrap();
        assert_eq!(
            cover.image_url,
            "https://editions.example/couv/9782000000013.jpg"
        );
    }

    #[tokio::test]
    async fn no_image_gone_images_and_a_silent_host_are_told_apart() {
        let site = FakeSite::default();
        assert_eq!(
            page_cover(&site, &page_with(&[]), ISBN).await.unwrap_err(),
            PageCoverError::NoImage
        );
        let gone = "https://editions.example/couv/9782000000013.jpg";
        assert_eq!(
            page_cover(&site, &page_with(&[gone]), ISBN)
                .await
                .unwrap_err(),
            PageCoverError::Broken
        );
        site.offline
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            page_cover(&site, &page_with(&[gone]), ISBN)
                .await
                .unwrap_err(),
            PageCoverError::Unreachable
        );
    }
}
