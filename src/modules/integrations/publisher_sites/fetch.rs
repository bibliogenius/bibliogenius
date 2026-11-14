//! How the probe (and later the app) reaches a publisher's site: through
//! [`SiteFetcher`], so the probe can be tested against an in-memory site, and
//! in production through [`PoliteFetcher`], which honours robots.txt, keeps a
//! pause between two requests to the same host, and tells a site that turns
//! scripts away from one that fails.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

use super::page::bare_host;
use super::robots::Robots;
use crate::utils::public_fetch::{
    self, BodyCap, FetchPolicy, FetchedBody, HopOutcome, MAX_REDIRECTS, PublicFetchError,
    upgrade_to_https,
};

/// Longest part of a page read: book pages carry what we need in their head
/// and first screens.
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// Longest robots.txt read (Google stops at 500 KiB).
const MAX_ROBOTS_BYTES: usize = 512 * 1024;

/// A firewall's rejection page (F5 "Request Rejected") is a few hundred bytes.
const REJECTION_PAGE_MAX_BYTES: usize = 600;

/// A fetched page, decoded.
#[derive(Debug, Clone)]
pub struct Page {
    /// Where the page was found, after redirects.
    pub url: Url,
    pub html: String,
    /// Cut at [`MAX_PAGE_BYTES`]: what came after is missing.
    pub truncated: bool,
}

/// What a submitted form answered.
#[derive(Debug, Clone)]
pub enum PostAnswer {
    /// The server redirects to this (not fetched) address.
    Redirect(Url),
    Page(Page),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The site refuses scripts (403, firewall page, rate limit).
    Blocked(String),
    /// The site's robots.txt forbids this address to us.
    Disallowed,
    /// Any other HTTP error status.
    Status(u16),
    /// Not reachable: DNS, connection, timeout, too many redirects.
    Network,
    /// Not an address we may fetch (scheme, private host, size).
    Refused,
}

#[async_trait]
pub trait SiteFetcher: Send + Sync {
    async fn get(&self, url: &Url) -> Result<Page, FetchError>;
    async fn post_form(
        &self,
        url: &Url,
        fields: &[(String, String)],
    ) -> Result<PostAnswer, FetchError>;
    /// The site maps the robots.txt of `site` declares. Empty when it declares
    /// none or cannot be read.
    async fn sitemaps(&self, site: &Url) -> Vec<Url>;
}

/// The production fetcher: public https only (an http address is tried over
/// https), robots.txt honoured, one request at a time per host with at least
/// `min_interval` (or the site's Crawl-delay) between two.
pub struct PoliteFetcher {
    min_interval: Duration,
    robots: Mutex<HashMap<String, Arc<Robots>>>,
    hosts: Mutex<HashMap<String, Arc<Mutex<Option<Instant>>>>>,
}

impl PoliteFetcher {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            robots: Mutex::new(HashMap::new()),
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// The robots.txt rules of `url`'s site, read once per run.
    pub async fn robots_for(&self, url: &Url) -> Result<Arc<Robots>, FetchError> {
        let origin = url.origin().ascii_serialization();
        if let Some(r) = self.robots.lock().await.get(&origin) {
            return Ok(r.clone());
        }
        let robots_url = url.join("/robots.txt").map_err(|_| FetchError::Refused)?;
        self.wait_turn(&robots_url, None).await;
        let robots = match public_fetch::fetch_with_policy(
            robots_url,
            &|_| BodyCap::Truncate(MAX_ROBOTS_BYTES),
            FetchPolicy::PublicInternet,
        )
        .await
        {
            Ok(body) => {
                let text = String::from_utf8_lossy(&body.bytes);
                if is_rejection_page(&body.bytes) {
                    // A firewall page instead of the file: read as no file.
                    Robots::allow_all()
                } else {
                    Robots::parse(&text)
                }
            }
            // Rate-limited on robots.txt itself: the site is turning us
            // away, not saying it has no rules. Not cached.
            Err(PublicFetchError::HttpStatus(429)) => {
                return Err(FetchError::Blocked("HTTP 429 on robots.txt".into()));
            }
            Err(PublicFetchError::HttpStatus(s)) if (400..500).contains(&s) => Robots::allow_all(),
            Err(PublicFetchError::HttpStatus(_)) => Robots::unreachable(),
            // Not cached: the site itself is unreachable, which the request
            // that follows will report.
            Err(e) => return Err(map_error(e)),
        };
        let robots = Arc::new(robots);
        self.robots.lock().await.insert(origin, robots.clone());
        Ok(robots)
    }

    /// Sleep until this host may be asked again, then book the slot.
    async fn wait_turn(&self, url: &Url, crawl_delay: Option<Duration>) {
        let slot = self
            .hosts
            .lock()
            .await
            .entry(pace_key(url))
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut last = slot.lock().await;
        let pause = crawl_delay.unwrap_or_default().max(self.min_interval);
        if let Some(at) = *last {
            tokio::time::sleep_until(at + pause).await;
        }
        *last = Some(Instant::now());
    }

    /// Checks made before every request, redirect hops included: https,
    /// robots.txt, our turn on the host. A robots.txt the server failed to
    /// serve (5xx) makes the site unavailable, not the address forbidden.
    async fn prepare(&self, url: &Url) -> Result<Url, FetchError> {
        let url = upgrade_to_https(url.clone()).ok_or(FetchError::Refused)?;
        let robots = self.robots_for(&url).await?;
        if robots.is_unreachable() {
            return Err(FetchError::Network);
        }
        if !robots.allows(&url) {
            return Err(FetchError::Disallowed);
        }
        self.wait_turn(&url, robots.crawl_delay()).await;
        Ok(url)
    }
}

#[async_trait]
impl SiteFetcher for PoliteFetcher {
    async fn get(&self, url: &Url) -> Result<Page, FetchError> {
        // Redirects are followed here, one hop at a time, so that robots.txt
        // and the pause apply to every address (another host included), not
        // only to the first.
        let mut next = url.clone();
        for _hop in 0..=MAX_REDIRECTS {
            let url = self.prepare(&next).await?;
            match public_fetch::get_one_hop_with_policy(
                url,
                &|_| BodyCap::Truncate(MAX_PAGE_BYTES),
                FetchPolicy::PublicInternet,
            )
            .await
            .map_err(map_error)?
            {
                HopOutcome::Redirect(to) => next = to,
                HopOutcome::Body(body) => return into_page(body),
            }
        }
        Err(FetchError::Network)
    }

    async fn post_form(
        &self,
        url: &Url,
        fields: &[(String, String)],
    ) -> Result<PostAnswer, FetchError> {
        let url = self.prepare(url).await?;
        match public_fetch::post_form_with_policy(
            url,
            fields,
            &|_| BodyCap::Truncate(MAX_PAGE_BYTES),
            FetchPolicy::PublicInternet,
        )
        .await
        .map_err(map_error)?
        {
            HopOutcome::Redirect(to) => Ok(PostAnswer::Redirect(to)),
            HopOutcome::Body(body) => into_page(body).map(PostAnswer::Page),
        }
    }

    async fn sitemaps(&self, site: &Url) -> Vec<Url> {
        let Some(site) = upgrade_to_https(site.clone()) else {
            return Vec::new();
        };
        match self.robots_for(&site).await {
            Ok(robots) => robots
                .sitemaps()
                .iter()
                .filter_map(|s| site.join(s).ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// The host requests are paced on: `www.` or not, it is one server.
fn pace_key(url: &Url) -> String {
    bare_host(url)
}

fn map_error(e: PublicFetchError) -> FetchError {
    match e {
        PublicFetchError::HttpStatus(s @ (403 | 429)) => FetchError::Blocked(format!("HTTP {s}")),
        PublicFetchError::HttpStatus(s) => FetchError::Status(s),
        PublicFetchError::Network | PublicFetchError::TooManyRedirects => FetchError::Network,
        PublicFetchError::InvalidUrl
        | PublicFetchError::BlockedAddress
        | PublicFetchError::TooLarge => FetchError::Refused,
    }
}

fn is_rejection_page(bytes: &[u8]) -> bool {
    bytes.len() < REJECTION_PAGE_MAX_BYTES && bytes.windows(16).any(|w| w == b"Request Rejected")
}

fn into_page(body: FetchedBody) -> Result<Page, FetchError> {
    if is_rejection_page(&body.bytes) {
        return Err(FetchError::Blocked("firewall rejection page".into()));
    }
    Ok(Page {
        truncated: body.bytes.len() >= MAX_PAGE_BYTES,
        html: decode_html(&body.bytes, body.content_type.as_deref()),
        url: body.final_url,
    })
}

/// Code points of windows-1252 bytes 0x80 to 0x9F (the rest is Latin-1);
/// unassigned bytes keep their Latin-1 value.
const WINDOWS_1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// A legacy single-byte charset older publisher sites still declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Legacy {
    Latin9,
    Windows1252,
}

fn legacy_char(b: u8, charset: Legacy) -> char {
    match (charset, b) {
        (Legacy::Windows1252, 0x80..=0x9f) => WINDOWS_1252_HIGH[usize::from(b - 0x80)],
        (Legacy::Latin9, 0xa4) => '€',
        (Legacy::Latin9, 0xa6) => 'Š',
        (Legacy::Latin9, 0xa8) => 'š',
        (Legacy::Latin9, 0xb4) => 'Ž',
        (Legacy::Latin9, 0xb8) => 'ž',
        (Legacy::Latin9, 0xbc) => 'Œ',
        (Legacy::Latin9, 0xbd) => 'œ',
        (Legacy::Latin9, 0xbe) => 'Ÿ',
        _ => char::from(b),
    }
}

/// Decode a page as UTF-8, or in the legacy charset its header or its first
/// bytes declare (lossy UTF-8 would turn every accented title into
/// replacement characters). A page that is valid UTF-8 stays UTF-8 whatever
/// it declares.
pub fn decode_html(bytes: &[u8], content_type: Option<&str>) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let declared = content_type.unwrap_or("").to_ascii_lowercase() + " " + &head;
    let says = |name: &str| {
        declared.contains(&format!("charset={name}"))
            || declared.contains(&format!("charset=\"{name}"))
    };
    // Browsers read a Latin-1 label as windows-1252, which only adds
    // characters in a range Latin-1 leaves to control codes.
    let charset = if says("iso-8859-15") || says("latin9") {
        Some(Legacy::Latin9)
    } else if ["windows-1252", "iso-8859-1", "latin1"]
        .iter()
        .any(|c| says(c))
    {
        Some(Legacy::Windows1252)
    } else {
        None
    };
    match charset {
        Some(cs) => bytes.iter().map(|&b| legacy_char(b, cs)).collect(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_declared_latin1_page() {
        let bytes = b"<meta charset=iso-8859-1><h1>L'\xeele des \xe2mes</h1>";
        assert_eq!(
            decode_html(bytes, None),
            "<meta charset=iso-8859-1><h1>L'île des âmes</h1>"
        );
        assert_eq!(
            decode_html(
                "<h1>L'île</h1>".as_bytes(),
                Some("text/html; charset=iso-8859-1")
            ),
            "<h1>L'île</h1>",
            "a page that is valid UTF-8 despite its header stays UTF-8"
        );
    }

    #[test]
    fn decodes_windows_1252_and_latin9_ligatures() {
        assert_eq!(
            decode_html(b"<meta charset=windows-1252>\x9cuvre \x92", None),
            "<meta charset=windows-1252>œuvre ’"
        );
        assert_eq!(
            decode_html(b"<meta charset=iso-8859-15>\xbduvre \xa4", None),
            "<meta charset=iso-8859-15>œuvre €"
        );
    }

    #[test]
    fn spots_a_firewall_rejection_page() {
        assert!(is_rejection_page(
            b"<html><title>Request Rejected</title></html>"
        ));
        assert!(!is_rejection_page(&[b'a'; 700]));
    }

    #[test]
    fn www_and_bare_host_share_one_pace() {
        let key = |u: &str| pace_key(&Url::parse(u).unwrap());
        assert_eq!(
            key("https://www.gallmeister.fr/livres/x"),
            key("https://gallmeister.fr/")
        );
        assert_ne!(key("https://gallmeister.fr/"), key("https://gallimard.fr/"));
    }

    #[test]
    fn rate_limits_and_403_mean_blocked() {
        assert!(matches!(
            map_error(PublicFetchError::HttpStatus(403)),
            FetchError::Blocked(_)
        ));
        assert!(matches!(
            map_error(PublicFetchError::HttpStatus(429)),
            FetchError::Blocked(_)
        ));
        assert_eq!(
            map_error(PublicFetchError::HttpStatus(404)),
            FetchError::Status(404)
        );
    }
}
