//! How the probe (and later the app) reaches a publisher's site: through
//! [`SiteFetcher`], so the probe can be tested against an in-memory site, and
//! in production through [`PoliteFetcher`], which honours robots.txt, keeps a
//! pause between two requests to the same host, and tells a site that turns
//! scripts away from one that fails.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

use super::page::bare_host;
use super::progress::{LookupStep, WAIT_WORTH_SAYING, report, resume, seconds_of};
use super::robots::Robots;
use crate::utils::cover_image::COVER_MAX_INPUT_BYTES;
use crate::utils::public_fetch::{
    self, BodyCap, FetchPolicy, FetchedBody, HopOutcome, MAX_REDIRECTS, PublicFetchError,
    upgrade_to_https,
};

/// Longest part of a page read: book pages carry what we need in their head
/// and first screens.
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// Longest robots.txt read (Google stops at 500 KiB).
const MAX_ROBOTS_BYTES: usize = 512 * 1024;

/// How long a robots.txt read is used: a day, the longest RFC 9309 (2.4)
/// allows. A fetcher kept for the life of the app reads it again after that.
const ROBOTS_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Most sites a fetcher keeps rules, refusals or pacing for. A lookup
/// touches a handful; a fetcher kept for the life of the app forgets the
/// oldest past this.
const MAX_SITES_KEPT: usize = 512;

/// A host idle this long loses its pacing state when the fetcher is full:
/// longer than any pause it could still owe.
const IDLE_HOST: Duration = Duration::from_secs(10 * 60);

/// How long ago `at` was, by the wall clock: memories measured in hours
/// must count a laptop's night asleep, which the monotonic clocks skip. A
/// clock set back makes it 0, and the memory younger: kept, never lost.
fn age(at: SystemTime) -> Duration {
    SystemTime::now().duration_since(at).unwrap_or_default()
}

/// Drop the oldest entries of `map` until it holds at most `max`.
fn keep_newest<V>(map: &mut HashMap<String, V>, max: usize, when: impl Fn(&V) -> SystemTime) {
    while map.len() > max {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, v)| when(v))
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        map.remove(&oldest);
    }
}

/// What a plain 403 is reported as: [`FetchError::Blocked`], but not a
/// reason to leave the whole host alone (see `PoliteFetcher::noted`).
const FORBIDDEN: &str = "HTTP 403";

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
    /// An image's raw bytes, up to the cover input cap, under the same rules
    /// as a page: robots.txt and the pause apply to every hop.
    async fn get_image(&self, url: &Url) -> Result<Vec<u8>, FetchError>;
    /// Whether robots.txt lets us fetch `url`, without fetching it. `Err`
    /// when the rules cannot be had: the site refuses us (`Blocked`) or does
    /// not answer (`Network`).
    async fn may_fetch(&self, url: &Url) -> Result<bool, FetchError>;
}

/// The production fetcher: public https only (an http address is tried over
/// https), robots.txt honoured, one request at a time per host with at least
/// `min_interval` (or the site's Crawl-delay) between two.
///
/// A host that turned us away once (429, a firewall's rejection page, a
/// Crawl-delay over the cap) is not asked again by this fetcher, for as long
/// as it lives or for [`Self::with_refusal_memory`]: over a run of many
/// books, asking again is what gets an address banned.
///
/// What it keeps per site is bounded ([`MAX_SITES_KEPT`]): it may live as
/// long as the app.
pub struct PoliteFetcher {
    min_interval: Duration,
    /// Longest Crawl-delay we wait for; a site asking more is left alone.
    max_crawl_delay: Option<Duration>,
    /// How long a refusal is remembered; `None`, as long as the fetcher.
    refusal_memory: Option<Duration>,
    /// Each site's rules, with when they were read.
    robots: Mutex<HashMap<String, (SystemTime, Arc<Robots>)>>,
    hosts: Mutex<HashMap<String, Arc<Mutex<Option<Instant>>>>>,
    /// Hosts (as [`pace_key`] names them) that turned us away, and when.
    refused: std::sync::Mutex<HashMap<String, SystemTime>>,
}

impl PoliteFetcher {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            max_crawl_delay: None,
            refusal_memory: None,
            robots: Mutex::new(HashMap::new()),
            hosts: Mutex::new(HashMap::new()),
            refused: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Ask a host that turned us away again once `memory` has passed: for a
    /// fetcher kept for the life of the app, where a rate limit lifted an
    /// hour later should not leave the site out until the next start.
    pub fn with_refusal_memory(mut self, memory: Duration) -> Self {
        self.refusal_memory = Some(memory);
        self
    }

    /// Leave alone the sites whose Crawl-delay is longer than `max`, instead
    /// of waiting for them: behind a click, nobody waits a minute between
    /// two requests. The site is reported as refusing us.
    pub fn with_max_crawl_delay(mut self, max: Duration) -> Self {
        self.max_crawl_delay = Some(max);
        self
    }

    fn refused_before(&self, url: &Url) -> bool {
        let Ok(mut hosts) = self.refused.lock() else {
            return false;
        };
        let key = pace_key(url);
        match hosts.get(&key) {
            None => false,
            Some(at) if self.refusal_memory.is_none_or(|m| age(*at) < m) => true,
            Some(_) => {
                hosts.remove(&key);
                false
            }
        }
    }

    /// Remember that `url`'s host turned us away, as a lookup would.
    #[cfg(test)]
    pub(crate) fn refuse_for_test(&self, url: &Url) {
        let _ = self.noted::<()>(url, Err(FetchError::Blocked("HTTP 429".into())));
    }

    /// `result`, after remembering that `url`'s host turned us away if it
    /// did in a way that asking again makes worse: a rate limit, a
    /// firewall's rejection page, a Crawl-delay over the cap. Not a plain
    /// 403, which a CDN answers for one missing image and some sites for one
    /// page: it rules that address out, not the host.
    fn noted<T>(&self, url: &Url, result: Result<T, FetchError>) -> Result<T, FetchError> {
        if let Err(FetchError::Blocked(why)) = &result
            && why != FORBIDDEN
            && let Ok(mut hosts) = self.refused.lock()
        {
            hosts.insert(pace_key(url), SystemTime::now());
            keep_newest(&mut hosts, MAX_SITES_KEPT, |at| *at);
        }
        result
    }

    /// `origin`'s rules when read less than [`ROBOTS_MAX_AGE`] ago (`Ok`),
    /// or the older copy there is, if any (`Err`).
    async fn cached_robots(&self, origin: &str) -> Result<Arc<Robots>, Option<Arc<Robots>>> {
        match self.robots.lock().await.get(origin) {
            Some((read, r)) if age(*read) < ROBOTS_MAX_AGE => Ok(r.clone()),
            Some((_, r)) => Err(Some(r.clone())),
            None => Err(None),
        }
    }

    /// The robots.txt rules of `url`'s site, read once per run, and again
    /// once [`ROBOTS_MAX_AGE`] old.
    pub async fn robots_for(&self, url: &Url) -> Result<Arc<Robots>, FetchError> {
        let origin = url.origin().ascii_serialization();
        if let Ok(fresh) = self.cached_robots(&origin).await {
            return Ok(fresh);
        }
        let robots_url = url.join("/robots.txt").map_err(|_| FetchError::Refused)?;
        let turn = self.wait_turn(&robots_url, None).await;
        // Another lookup may have read them while we waited for the host:
        // one read per site, and no pause owed for a request not made.
        let older = match self.cached_robots(&origin).await {
            Ok(fresh) => {
                turn.unused();
                return Ok(fresh);
            }
            Err(older) => older,
        };
        report(LookupStep::Rules {
            site: pace_key(url),
        });
        let fetched = public_fetch::fetch_with_policy(
            robots_url,
            &|_| BodyCap::Truncate(MAX_ROBOTS_BYTES),
            FetchPolicy::PublicInternet,
        )
        .await;
        drop(turn);
        let robots = match rules_from(fetched, older)? {
            Rules::Read(robots) => Arc::new(robots),
            Rules::ForNow(robots) => return Ok(robots),
        };
        let mut cache = self.robots.lock().await;
        cache.insert(origin, (SystemTime::now(), robots.clone()));
        keep_newest(&mut cache, MAX_SITES_KEPT, |(read, _)| *read);
        Ok(robots)
    }

    /// Wait for this host's turn: no request of ours in flight there, and
    /// the pause since the last one ended. The turn is held until the
    /// returned [`Turn`] is dropped, after the request: one request at a
    /// time per host, however slow it answers.
    async fn wait_turn(&self, url: &Url, crawl_delay: Option<Duration>) -> Turn {
        let slot = {
            let mut hosts = self.hosts.lock().await;
            let slot = hosts
                .entry(pace_key(url))
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone();
            forget_idle_hosts(&mut hosts);
            slot
        };
        let mut waited = false;
        let last = match slot.clone().try_lock_owned() {
            Ok(last) => last,
            Err(_) => {
                report(LookupStep::Queued {
                    site: pace_key(url),
                });
                waited = true;
                slot.lock_owned().await
            }
        };
        let pause = crawl_delay.unwrap_or_default().max(self.min_interval);
        if let Some(at) = *last {
            let left = (at + pause).saturating_duration_since(Instant::now());
            if left >= WAIT_WORTH_SAYING {
                report(LookupStep::Waiting {
                    site: pace_key(url),
                    seconds: seconds_of(left),
                });
                waited = true;
            }
            tokio::time::sleep_until(at + pause).await;
        }
        if waited {
            resume();
        }
        Turn { last, used: true }
    }

    /// Checks made before every request, redirect hops included: https,
    /// robots.txt, our turn on the host. A robots.txt the server failed to
    /// serve (5xx) makes the site unavailable, not the address forbidden.
    async fn prepare(&self, url: &Url) -> Result<(Url, Turn), FetchError> {
        let url = upgrade_to_https(url.clone()).ok_or(FetchError::Refused)?;
        if self.refused_before(&url) {
            return Err(FetchError::Blocked("refused us earlier".into()));
        }
        let robots = self.noted(&url, self.robots_for(&url).await)?;
        if robots.is_unreachable() {
            return Err(FetchError::Network);
        }
        if !robots.allows(&url) {
            return Err(FetchError::Disallowed);
        }
        let delay = robots.crawl_delay();
        if let (Some(delay), Some(max)) = (delay, self.max_crawl_delay)
            && delay > max
        {
            return self.noted(
                &url,
                Err(FetchError::Blocked(format!(
                    "Crawl-delay of {} s",
                    delay.as_secs()
                ))),
            );
        }
        let turn = self.wait_turn(&url, delay).await;
        Ok((url, turn))
    }

    /// GET `url`, its redirects followed here, one hop at a time, so that
    /// robots.txt and the pause apply to every address (another host
    /// included), not only to the first.
    async fn follow(&self, url: &Url, cap: BodyCap) -> Result<FetchedBody, FetchError> {
        let mut next = url.clone();
        for _hop in 0..=MAX_REDIRECTS {
            let (url, turn) = self.prepare(&next).await?;
            let hop = public_fetch::get_one_hop_with_policy(
                url.clone(),
                &|_| cap,
                FetchPolicy::PublicInternet,
            )
            .await;
            drop(turn);
            let hop = hop.map_err(map_error).and_then(|hop| match hop {
                HopOutcome::Body(body) if is_rejection_page(&body.bytes) => {
                    Err(FetchError::Blocked("firewall rejection page".into()))
                }
                hop => Ok(hop),
            });
            match self.noted(&url, hop)? {
                HopOutcome::Redirect(to) => next = to,
                HopOutcome::Body(body) => return Ok(body),
            }
        }
        Err(FetchError::Network)
    }
}

/// What a robots.txt answer gives.
#[derive(Debug)]
enum Rules {
    /// Rules to keep for [`ROBOTS_MAX_AGE`].
    Read(Robots),
    /// Rules for this request only, not cached.
    ForNow(Arc<Robots>),
}

/// The rules a robots.txt answer gives, `older` being the copy we hold from
/// an earlier read, if any.
fn rules_from(
    fetched: Result<FetchedBody, PublicFetchError>,
    older: Option<Arc<Robots>>,
) -> Result<Rules, FetchError> {
    match fetched {
        Ok(body) => read_robots(&body.bytes).map(Rules::Read),
        // Rate-limited on robots.txt itself: the site is turning us away,
        // not saying it has no rules. Not cached.
        Err(PublicFetchError::HttpStatus(429)) => {
            Err(FetchError::Blocked("HTTP 429 on robots.txt".into()))
        }
        Err(PublicFetchError::HttpStatus(s)) if (400..500).contains(&s) => {
            Ok(Rules::Read(Robots::allow_all()))
        }
        // The server fails to serve its rules: everything is forbidden
        // meanwhile (RFC 9309, 2.3.1.4), unless we hold an older copy, which
        // a crawler may keep using while the file is unreachable (2.4). Not
        // cached either way: a passing 503 must not shut the site for a day,
        // nor replace rules that were read.
        Err(PublicFetchError::HttpStatus(_)) => Ok(Rules::ForNow(
            older.unwrap_or_else(|| Arc::new(Robots::unreachable())),
        )),
        // Not cached: the site itself is unreachable, which the request that
        // follows will report.
        Err(e) => Err(map_error(e)),
    }
}

/// Forget the pacing of hosts idle for [`IDLE_HOST`] once the fetcher holds
/// more than [`MAX_SITES_KEPT`]; a host with a request in flight or waiting
/// is kept.
fn forget_idle_hosts(hosts: &mut HashMap<String, Arc<Mutex<Option<Instant>>>>) {
    if hosts.len() <= MAX_SITES_KEPT {
        return;
    }
    hosts.retain(|_, slot| {
        Arc::strong_count(slot) > 1
            || slot
                .try_lock()
                .map_or(true, |last| last.is_some_and(|at| at.elapsed() < IDLE_HOST))
    });
}

/// A host's turn, held while our request to it is in flight. Dropped, it
/// records when the request ended, which the next pause counts from.
struct Turn {
    last: tokio::sync::OwnedMutexGuard<Option<Instant>>,
    used: bool,
}

impl Turn {
    /// Give the turn back without a request: no pause is owed for it.
    fn unused(mut self) {
        self.used = false;
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        if self.used {
            *self.last = Some(Instant::now());
        }
    }
}

#[async_trait]
impl SiteFetcher for PoliteFetcher {
    async fn get(&self, url: &Url) -> Result<Page, FetchError> {
        into_page(self.follow(url, BodyCap::Truncate(MAX_PAGE_BYTES)).await?)
    }

    async fn get_image(&self, url: &Url) -> Result<Vec<u8>, FetchError> {
        let body = self
            .follow(url, BodyCap::Refuse(COVER_MAX_INPUT_BYTES))
            .await?;
        Ok(body.bytes)
    }

    async fn may_fetch(&self, url: &Url) -> Result<bool, FetchError> {
        let url = upgrade_to_https(url.clone()).ok_or(FetchError::Refused)?;
        if self.refused_before(&url) {
            return Err(FetchError::Blocked("refused us earlier".into()));
        }
        let robots = self.noted(&url, self.robots_for(&url).await)?;
        // A robots.txt the server fails to serve forbids everything
        // (RFC 9309, 2.3.1.4).
        Ok(!robots.is_unreachable() && robots.allows(&url))
    }

    async fn post_form(
        &self,
        url: &Url,
        fields: &[(String, String)],
    ) -> Result<PostAnswer, FetchError> {
        let (url, turn) = self.prepare(url).await?;
        let answer = public_fetch::post_form_with_policy(
            url.clone(),
            fields,
            &|_| BodyCap::Truncate(MAX_PAGE_BYTES),
            FetchPolicy::PublicInternet,
        )
        .await;
        drop(turn);
        let answer = answer.map_err(map_error).and_then(|hop| match hop {
            HopOutcome::Redirect(to) => Ok(PostAnswer::Redirect(to)),
            HopOutcome::Body(body) => into_page(body).map(PostAnswer::Page),
        });
        self.noted(&url, answer)
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
        PublicFetchError::HttpStatus(403) => FetchError::Blocked(FORBIDDEN.into()),
        PublicFetchError::HttpStatus(429) => FetchError::Blocked("HTTP 429".into()),
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

/// The rules a robots.txt body gives. A firewall's rejection page in its
/// place is the site turning us away, as a 429 there is: not cached, and
/// no rules inferred from it (the F5 firewalls met reject the whole
/// address, robots.txt included).
fn read_robots(bytes: &[u8]) -> Result<Robots, FetchError> {
    if is_rejection_page(bytes) {
        return Err(FetchError::Blocked(
            "firewall rejection page on robots.txt".into(),
        ));
    }
    Ok(Robots::parse(&String::from_utf8_lossy(bytes)))
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
    fn a_rejection_page_for_robots_txt_turns_us_away_instead_of_allowing_all() {
        assert!(matches!(
            read_robots(b"<html><title>Request Rejected</title></html>"),
            Err(FetchError::Blocked(_))
        ));
        let robots = read_robots(b"User-agent: *\nDisallow: /panier\n").unwrap();
        assert!(!robots.allows(&Url::parse("https://editions.example/panier").unwrap()));
        assert!(robots.allows(&Url::parse("https://editions.example/livre").unwrap()));
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

    /// A fetcher that already holds `robots` for editions.example, a name
    /// that resolves nowhere: a test reaching the network gets `Network`.
    async fn with_robots(robots: &str) -> PoliteFetcher {
        let fetcher = PoliteFetcher::new(Duration::from_millis(10));
        fetcher.robots.lock().await.insert(
            "https://editions.example".into(),
            (SystemTime::now(), Arc::new(Robots::parse(robots))),
        );
        fetcher
    }

    fn hours_ago(hours: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(hours * 60 * 60)
    }

    #[tokio::test]
    async fn a_refusal_is_forgotten_after_the_memory_given() {
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let refused = Err::<(), _>(FetchError::Blocked("HTTP 429".into()));
        // By default, for as long as the fetcher lives: a probe run.
        let run = with_robots("User-agent: *\n").await;
        let _ = run.noted(&page, refused.clone());
        run.refused
            .lock()
            .unwrap()
            .insert("editions.example".into(), hours_ago(48));
        assert!(run.refused_before(&page));

        let app = with_robots("User-agent: *\n")
            .await
            .with_refusal_memory(Duration::from_secs(60 * 60));
        let _ = app.noted(&page, refused);
        assert!(app.refused_before(&page));
        // Two hours later by the wall clock, a laptop's sleep included.
        app.refused
            .lock()
            .unwrap()
            .insert("editions.example".into(), hours_ago(2));
        assert!(!app.refused_before(&page));
        // Asked again, the host is reached (and found missing).
        assert_eq!(app.get(&page).await.unwrap_err(), FetchError::Network);
    }

    #[tokio::test]
    async fn robots_txt_is_read_again_after_a_day() {
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let fetcher = with_robots("User-agent: *\nDisallow: /livre\n").await;
        assert_eq!(
            fetcher.get(&page).await.unwrap_err(),
            FetchError::Disallowed
        );
        fetcher
            .robots
            .lock()
            .await
            .get_mut("https://editions.example")
            .unwrap()
            .0 = hours_ago(25);
        // The old rules are not used: robots.txt is asked for again (and
        // the host is not found).
        assert_eq!(fetcher.get(&page).await.unwrap_err(), FetchError::Network);
    }

    #[test]
    fn a_server_failing_robots_txt_is_not_remembered() {
        let read = Arc::new(Robots::parse("User-agent: *\nDisallow: /panier\n"));
        // A 503 with rules read before: those, for now.
        match rules_from(Err(PublicFetchError::HttpStatus(503)), Some(read.clone())) {
            Ok(Rules::ForNow(r)) => assert!(Arc::ptr_eq(&r, &read)),
            other => panic!("{other:?}"),
        }
        // Without: everything forbidden, for now only.
        match rules_from(Err(PublicFetchError::HttpStatus(503)), None) {
            Ok(Rules::ForNow(r)) => assert!(r.is_unreachable()),
            other => panic!("{other:?}"),
        }
        // A 404 says there are no rules: kept.
        assert!(matches!(
            rules_from(Err(PublicFetchError::HttpStatus(404)), None),
            Ok(Rules::Read(_))
        ));
        assert!(matches!(
            rules_from(Err(PublicFetchError::HttpStatus(429)), Some(read)),
            Err(FetchError::Blocked(_))
        ));
    }

    #[tokio::test]
    async fn robots_txt_read_while_we_waited_is_not_read_again() {
        let fetcher = Arc::new(PoliteFetcher::new(Duration::from_millis(10)));
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let held = fetcher.wait_turn(&page, None).await;
        let waiting = {
            let fetcher = fetcher.clone();
            let page = page.clone();
            tokio::spawn(async move { fetcher.robots_for(&page).await })
        };
        tokio::task::yield_now().await;
        // Another lookup reads them meanwhile.
        fetcher.robots.lock().await.insert(
            "https://editions.example".into(),
            (
                SystemTime::now(),
                Arc::new(Robots::parse("User-agent: *\nDisallow: /panier\n")),
            ),
        );
        drop(held);
        // No request (editions.example resolves nowhere: one would fail).
        let robots = waiting.await.unwrap().unwrap();
        assert!(!robots.allows(&Url::parse("https://editions.example/panier").unwrap()));
    }

    #[test]
    fn what_a_fetcher_keeps_is_bounded() {
        let mut refused: HashMap<String, SystemTime> = (0..MAX_SITES_KEPT + 3)
            .map(|i| (format!("site{i}.example"), hours_ago(i as u64)))
            .collect();
        keep_newest(&mut refused, MAX_SITES_KEPT, |at| *at);
        assert_eq!(refused.len(), MAX_SITES_KEPT);
        assert!(refused.contains_key("site0.example"), "the newest stay");
        assert!(!refused.contains_key(&format!("site{}.example", MAX_SITES_KEPT + 2)));

        let mut hosts: HashMap<String, Arc<Mutex<Option<Instant>>>> = (0..MAX_SITES_KEPT + 1)
            .map(|i| (format!("site{i}.example"), Arc::new(Mutex::new(None))))
            .collect();
        let busy = hosts["site0.example"].clone();
        *hosts["site1.example"].try_lock().unwrap() = Some(Instant::now());
        forget_idle_hosts(&mut hosts);
        assert_eq!(hosts.len(), 2, "in use, or asked recently");
        drop(busy);
    }

    #[tokio::test]
    async fn a_host_that_refused_us_is_not_asked_again() {
        let fetcher = with_robots("User-agent: *\nAllow: /\n").await;
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        // Not refused yet: the request goes out (and finds no such host).
        assert_eq!(fetcher.get(&page).await.unwrap_err(), FetchError::Network);
        // A plain 403 rules one address out, not the host.
        let _ = fetcher.noted::<()>(&page, Err(FetchError::Blocked(FORBIDDEN.into())));
        assert!(!fetcher.refused_before(&page));
        let _ = fetcher.noted::<()>(&page, Err(FetchError::Blocked("HTTP 429".into())));
        let other = Url::parse("https://www.editions.example/livre/2").unwrap();
        assert!(matches!(
            fetcher.get(&other).await,
            Err(FetchError::Blocked(_))
        ));
        assert!(matches!(
            fetcher.may_fetch(&other).await,
            Err(FetchError::Blocked(_))
        ));
    }

    #[tokio::test]
    async fn a_crawl_delay_over_the_cap_leaves_the_site_alone() {
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let fetcher = with_robots("User-agent: *\nCrawl-delay: 30\n")
            .await
            .with_max_crawl_delay(Duration::from_secs(10));
        assert!(matches!(
            fetcher.get(&page).await,
            Err(FetchError::Blocked(_))
        ));
        assert!(fetcher.refused_before(&page));
        // Without a cap, the delay is waited for: the request goes out.
        let patient = with_robots("User-agent: *\nCrawl-delay: 30\n").await;
        assert_eq!(patient.get(&page).await.unwrap_err(), FetchError::Network);
    }

    #[tokio::test]
    async fn a_server_that_fails_robots_txt_forbids_every_image() {
        let fetcher = PoliteFetcher::new(Duration::from_millis(10));
        fetcher.robots.lock().await.insert(
            "https://editions.example".into(),
            (SystemTime::now(), Arc::new(Robots::unreachable())),
        );
        let image = Url::parse("https://editions.example/couv/1.jpg").unwrap();
        assert_eq!(fetcher.may_fetch(&image).await, Ok(false));
    }

    #[tokio::test]
    async fn a_wait_the_site_asks_for_is_said() {
        use super::super::progress::{LookupStep, reporting_to, testing::recorder};
        let fetcher = PoliteFetcher::new(Duration::from_millis(1500));
        let page = Url::parse("https://www.editions.example/livre/1").unwrap();
        let (reporter, steps) = recorder();
        reporting_to(reporter, async {
            drop(fetcher.wait_turn(&page, None).await);
            drop(fetcher.wait_turn(&page, None).await);
        })
        .await;
        // The first request goes at once; the second waits 1.5 s, said as 2.
        assert_eq!(
            *steps.lock().unwrap(),
            [LookupStep::Waiting {
                site: "editions.example".into(),
                seconds: 2
            }]
        );
    }

    #[tokio::test]
    async fn a_short_pause_is_not_said() {
        use super::super::progress::{reporting_to, testing::recorder};
        let fetcher = PoliteFetcher::new(Duration::from_millis(10));
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let (reporter, steps) = recorder();
        reporting_to(reporter, async {
            drop(fetcher.wait_turn(&page, None).await);
            drop(fetcher.wait_turn(&page, None).await);
        })
        .await;
        assert!(steps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn waiting_behind_another_request_is_said_then_the_work_again() {
        use super::super::progress::{LookupStep, report, reporting_to, testing::recorder};
        let fetcher = Arc::new(PoliteFetcher::new(Duration::from_millis(10)));
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let held = fetcher.wait_turn(&page, None).await;
        let (reporter, steps) = recorder();
        let second = {
            let fetcher = fetcher.clone();
            tokio::spawn(reporting_to(reporter, async move {
                report(LookupStep::Search {
                    site: "editions.example".into(),
                });
                drop(fetcher.wait_turn(&page, None).await);
            }))
        };
        tokio::task::yield_now().await;
        drop(held);
        second.await.unwrap();
        let site = || "editions.example".to_owned();
        assert_eq!(
            *steps.lock().unwrap(),
            [
                LookupStep::Search { site: site() },
                LookupStep::Queued { site: site() },
                LookupStep::Search { site: site() },
            ]
        );
    }

    #[tokio::test]
    async fn one_request_at_a_time_per_host() {
        let fetcher = PoliteFetcher::new(Duration::from_millis(10));
        let page = Url::parse("https://editions.example/livre/1").unwrap();
        let other = Url::parse("https://autre.example/").unwrap();
        let wait = Duration::from_millis(200);
        let first = fetcher.wait_turn(&page, None).await;
        // While the first request is in flight, the host is not asked again,
        // however long it takes; another host is.
        assert!(
            tokio::time::timeout(wait, fetcher.wait_turn(&page, None))
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(wait, fetcher.wait_turn(&other, None))
                .await
                .is_ok()
        );
        drop(first);
        assert!(
            tokio::time::timeout(wait, fetcher.wait_turn(&page, None))
                .await
                .is_ok()
        );
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
