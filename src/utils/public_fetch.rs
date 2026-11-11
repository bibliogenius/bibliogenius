//! Fetch a resource from the public internet at a URL the user typed.
//!
//! A user-supplied URL is an SSRF vector: without guards, pasting
//! `https://192.168.1.1/` or a name that resolves to a LAN address would make
//! the app probe the user's own network. `api::peer::helpers::validate_url` is
//! not usable here: it deliberately admits private ranges for P2P on a LAN.
//! This module admits only the public internet:
//!
//! - `https` only, on the default port, without credentials in the URL;
//! - the host must not be a local name (`localhost`, `*.local`, single-label);
//! - every address the name resolves to must be public (no loopback, private,
//!   link-local, CGNAT, ULA, documentation or reserved range), and the
//!   connection is pinned to the checked addresses, so a DNS answer that changes
//!   between the check and the connect (DNS rebinding) cannot redirect it;
//! - redirects are followed by hand, at most [`MAX_REDIRECTS`], and every hop
//!   goes through the same checks;
//! - no system proxy (it would resolve the name itself, out of our checks);
//! - bounded time, and a body cap chosen by the caller from the content type,
//!   enforced while streaming rather than trusted from `Content-Length`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use url::{Host, Url};

use crate::modules::integrations::API_USER_AGENT;

/// Redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 3;

/// Time allowed to open a connection, and without receiving a single byte
/// once connected: what tells a dead server from a slow link.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Whole-request ceiling. Generous on purpose: a 10 MB image over a rural
/// 1 Mbit/s link takes over a minute, and a steady transfer must not be cut.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a public fetch failed. Callers map these to user-facing messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicFetchError {
    /// Not an https URL we accept (bad syntax, other scheme, credentials).
    InvalidUrl,
    /// The URL points at, or resolves to, a non-public address or port.
    BlockedAddress,
    /// DNS failure, connection failure or timeout.
    Network,
    /// More than [`MAX_REDIRECTS`] redirects.
    TooManyRedirects,
    /// The server answered with a non-success status.
    HttpStatus(u16),
    /// The body exceeds the cap the caller set for its content type.
    TooLarge,
}

/// How much of a body a caller accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyCap {
    /// Over this many bytes the fetch fails with [`PublicFetchError::TooLarge`].
    Refuse(usize),
    /// Only the first bytes are kept, the rest is not read: an HTML page whose
    /// hints sit in its head need not be read to the end.
    Truncate(usize),
}

/// A fetched body and where it actually came from.
#[derive(Debug)]
pub struct FetchedBody {
    /// The URL that answered, after redirects.
    pub final_url: Url,
    /// The `Content-Type` header, lowercased, parameters included.
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// Which URLs a fetch may reach.
///
/// Production code only ever uses [`FetchPolicy::PublicInternet`]. The test
/// variant additionally admits plain-http loopback so the fetch loop can be
/// exercised against a local mock server; it does not exist outside tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchPolicy {
    PublicInternet,
    #[cfg(test)]
    PublicOrLoopbackForTests,
}

impl FetchPolicy {
    /// Check a URL against the policy, before any network access.
    pub(crate) fn check(self, url: &Url) -> Result<(), PublicFetchError> {
        #[cfg(test)]
        if self == FetchPolicy::PublicOrLoopbackForTests && is_test_loopback(url) {
            return Ok(());
        }
        check_public_https_url(url)
    }

    /// Whether this policy upgrades an `http` link found in a page to `https`.
    /// Under the test policy the mock server only speaks plain http.
    pub(crate) fn upgrades_http(self) -> bool {
        self == FetchPolicy::PublicInternet
    }
}

#[cfg(test)]
fn is_test_loopback(url: &Url) -> bool {
    url.scheme() == "http" && url.host() == Some(Host::Ipv4(Ipv4Addr::LOCALHOST))
}

/// Parse `raw` and check it is a public https URL, before any network access.
pub fn parse_public_https_url(raw: &str) -> Result<Url, PublicFetchError> {
    let url = Url::parse(raw.trim()).map_err(|_| PublicFetchError::InvalidUrl)?;
    check_public_https_url(&url)?;
    Ok(url)
}

fn check_public_https_url(url: &Url) -> Result<(), PublicFetchError> {
    if url.scheme() != "https" {
        return Err(PublicFetchError::InvalidUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PublicFetchError::InvalidUrl);
    }
    // `Url::port` is None for the scheme's default port.
    if url.port().is_some() {
        return Err(PublicFetchError::BlockedAddress);
    }
    match url.host() {
        None => Err(PublicFetchError::InvalidUrl),
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            let local_name = domain == "localhost"
                || domain.ends_with(".localhost")
                || domain.ends_with(".local")
                || domain.ends_with(".internal")
                || domain.ends_with(".lan")
                || domain.ends_with(".home.arpa")
                // A single label resolves through the LAN's search domain.
                || !domain.contains('.');
            if local_name {
                Err(PublicFetchError::BlockedAddress)
            } else {
                Ok(())
            }
        }
        Some(Host::Ipv4(ip)) => require_public(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => require_public(IpAddr::V6(ip)),
    }
}

fn require_public(ip: IpAddr) -> Result<(), PublicFetchError> {
    if is_public_ip(ip) {
        Ok(())
    } else {
        Err(PublicFetchError::BlockedAddress)
    }
}

/// True when `ip` is a globally routable unicast address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let blocked = a == 0 // "this network"
        || a == 10 // private
        || (a == 100 && (64..128).contains(&b)) // CGNAT shared space
        || a == 127 // loopback
        || (a == 169 && b == 254) // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b)) // private
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // documentation
        || (a == 192 && b == 168) // private
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // documentation
        || (a == 203 && b == 0 && c == 113) // documentation
        || a >= 224; // multicast, reserved, broadcast
    !blocked
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) embed an IPv4 address: judge
    // the embedded one, or a private v4 target slips through as "v6".
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return is_public_v4(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            s[6] as u8,
            (s[7] >> 8) as u8,
            s[7] as u8,
        ));
    }
    if s[0] == 0x2002 {
        return is_public_v4(Ipv4Addr::new(
            (s[1] >> 8) as u8,
            s[1] as u8,
            (s[2] >> 8) as u8,
            s[2] as u8,
        ));
    }
    // An allowlist: only global unicast (2000::/3) can be public, so a range
    // nobody listed (local-use NAT64 64:ff9b:1::/48, a future special block)
    // is refused rather than trusted.
    if (s[0] & 0xe000) != 0x2000 {
        return false;
    }
    let blocked = (s[0] == 0x2001 && s[1] == 0) // Teredo, tunnels to IPv4
        || (s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0) // benchmarking
        || (s[0] == 0x2001 && (s[1] & 0xfff0) == 0x0020) // ORCHIDv2
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x3fff && s[1] < 0x1000); // documentation (3fff::/20)
    !blocked
}

/// Where a redirect leads. A 3xx without a usable target (300, 304) is an
/// answer, not a hop. An old site bouncing through http is followed over
/// https, as an http link found in a page is; the next hop is then checked
/// like any other.
fn redirect_target(
    current: &Url,
    location: Option<&str>,
    status: u16,
    policy: FetchPolicy,
) -> Result<Url, PublicFetchError> {
    let next = location
        .and_then(|loc| current.join(loc).ok())
        .ok_or(PublicFetchError::HttpStatus(status))?;
    if policy.upgrades_http() {
        upgrade_to_https(next).ok_or(PublicFetchError::InvalidUrl)
    } else {
        Ok(next)
    }
}

/// Many older sites still link or redirect to `http://` while serving the same
/// path over https: the https form is tried rather than the address refused.
/// Anything else than http(s), or an explicit port that would not survive the
/// change, gives `None`.
pub(crate) fn upgrade_to_https(mut url: Url) -> Option<Url> {
    match url.scheme() {
        "https" => Some(url),
        "http" => {
            if url.port().is_some() {
                return None;
            }
            url.set_scheme("https").ok()?;
            Some(url)
        }
        _ => None,
    }
}

/// Resolve the URL's host and return its addresses, all of them public. IP
/// literals were already judged by the policy check. Refusing when ANY answer
/// is non-public (rather than keeping the public ones) keeps a name with mixed
/// answers from being half-trusted. All addresses are kept so the connection
/// can fall back from IPv6 to IPv4 (or the reverse) like a normal client.
async fn resolve_checked(url: &Url) -> Result<Vec<SocketAddr>, PublicFetchError> {
    let port = url
        .port_or_known_default()
        .ok_or(PublicFetchError::InvalidUrl)?;
    let domain = match url.host() {
        Some(Host::Ipv4(ip)) => return Ok(vec![SocketAddr::new(IpAddr::V4(ip), port)]),
        Some(Host::Ipv6(ip)) => return Ok(vec![SocketAddr::new(IpAddr::V6(ip), port)]),
        Some(Host::Domain(d)) => d.to_owned(),
        None => return Err(PublicFetchError::InvalidUrl),
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain.as_str(), port))
        .await
        .map_err(|_| PublicFetchError::Network)?
        .collect();
    if addrs.is_empty() {
        return Err(PublicFetchError::Network);
    }
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        tracing::warn!(target: "ssrf", "public fetch refused: {domain} resolves to a non-public address");
        return Err(PublicFetchError::BlockedAddress);
    }
    Ok(addrs)
}

/// Fetch `start` (checked with [`parse_public_https_url`] by the caller) under
/// `policy`, following redirects by hand.
///
/// `body_cap` picks the body cap from the response's content type (lowercased,
/// `None` when absent), so an HTML page and an image can be held to different
/// limits by one call. The policy is a parameter so tests can point the loop at
/// a loopback mock server.
pub(crate) async fn fetch_with_policy(
    start: Url,
    body_cap: &(dyn Fn(Option<&str>) -> BodyCap + Sync),
    policy: FetchPolicy,
) -> Result<FetchedBody, PublicFetchError> {
    let mut url = start;
    for _hop in 0..=MAX_REDIRECTS {
        policy.check(&url)?;
        let addrs = resolve_checked(&url).await?;

        let mut builder = reqwest::Client::builder()
            .user_agent(API_USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some(Host::Domain(domain)) = url.host() {
            // Pin the connection to the addresses checked above.
            builder = builder.resolve_to_addrs(domain, &addrs);
        }
        let client = builder.build().map_err(|_| PublicFetchError::Network)?;

        // Some CDNs close a connection before answering now and then. A GET
        // with no answer yet is safe to send once more; a second failure is
        // reported.
        let mut resp = match client.get(url.clone()).send().await {
            Ok(resp) => resp,
            Err(_) => client
                .get(url.clone())
                .send()
                .await
                .map_err(|_| PublicFetchError::Network)?,
        };
        let status = resp.status();

        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok());
            url = redirect_target(&url, location, status.as_u16(), policy)?;
            continue;
        }
        if !status.is_success() {
            return Err(PublicFetchError::HttpStatus(status.as_u16()));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase());
        let (cap, truncate) = match body_cap(content_type.as_deref()) {
            BodyCap::Refuse(cap) => (cap, false),
            BodyCap::Truncate(cap) => (cap, true),
        };
        if !truncate && resp.content_length().is_some_and(|len| len > cap as u64) {
            return Err(PublicFetchError::TooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| PublicFetchError::Network)? {
            if bytes.len() + chunk.len() > cap {
                if !truncate {
                    return Err(PublicFetchError::TooLarge);
                }
                bytes.extend_from_slice(&chunk[..cap - bytes.len()]);
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(FetchedBody {
            final_url: url,
            content_type,
            bytes,
        });
    }
    Err(PublicFetchError::TooManyRedirects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn parse(raw: &str) -> Result<Url, PublicFetchError> {
        parse_public_https_url(raw)
    }

    #[test]
    fn accepts_a_public_https_url() {
        let url = parse("  https://www.example-editions.fr/livre/42?ref=x  ").unwrap();
        assert_eq!(url.host_str(), Some("www.example-editions.fr"));
        assert!(parse("https://8.8.8.8/cover.jpg").is_ok());
        assert!(parse("https://example.org:443/a.jpg").is_ok());
    }

    #[test]
    fn rejects_anything_but_plain_https() {
        for raw in [
            "http://example.org/a.jpg",
            "ftp://example.org/a.jpg",
            "file:///etc/passwd",
            "data:image/png;base64,AAAA",
            "https://user:pass@example.org/a.jpg",
            "https://user@example.org/a.jpg",
            "not a url",
            "",
        ] {
            assert_eq!(parse(raw), Err(PublicFetchError::InvalidUrl), "{raw}");
        }
    }

    #[test]
    fn rejects_local_names_and_non_default_ports() {
        for raw in [
            "https://localhost/a.jpg",
            "https://LOCALHOST./a.jpg",
            "https://printer.local/a.jpg",
            "https://app.localhost/a.jpg",
            "https://nas.lan/a.jpg",
            "https://router.home.arpa/a.jpg",
            "https://metadata.google.internal/a.jpg",
            "https://intranet/a.jpg",
            "https://example.org:8443/a.jpg",
            "https://example.org:22/a.jpg",
        ] {
            assert_eq!(parse(raw), Err(PublicFetchError::BlockedAddress), "{raw}");
        }
    }

    #[test]
    fn rejects_non_public_ip_literals_in_every_spelling() {
        for raw in [
            "https://127.0.0.1/",
            "https://2130706433/", // decimal 127.0.0.1
            "https://0x7f.0.0.1/", // hex
            "https://10.1.2.3/",
            "https://172.16.0.1/",
            "https://192.168.1.1/",
            "https://169.254.169.254/latest/meta-data/",
            "https://100.64.0.1/",
            "https://0.0.0.0/",
            "https://[::1]/",
            "https://[::ffff:192.168.1.1]/",
            "https://[fd00::1]/",
            "https://[fe80::1]/",
            "https://[64:ff9b::a00:1]/",   // NAT64 of 10.0.0.1
            "https://[2002:c0a8:101::1]/", // 6to4 of 192.168.1.1
        ] {
            assert_eq!(parse(raw), Err(PublicFetchError::BlockedAddress), "{raw}");
        }
    }

    #[test]
    fn ipv6_is_public_only_inside_global_unicast() {
        // An allowlist, not a denylist: anything outside 2000::/3 is refused,
        // so a range nobody thought of cannot slip through as "public".
        for ip in [
            "64:ff9b:1::a00:1", // local-use NAT64, may front a private IPv4
            "2001::1",          // Teredo, tunnels to an IPv4 host
            "2001:0:4136:e378::1",
            "2001:2::1",  // benchmarking
            "2001:20::1", // ORCHIDv2
            "3fff::1",    // documentation (2024)
            "4000::1",    // unallocated, outside 2000::/3
            "1::1",
            "::ffff:0:a00:1", // IPv4-translated
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in [
            "2606:4700::1111",
            "2a00:1450:4007::1",
            "2001:4860:4860::8888",
            "2400:cb00::1",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn classifies_ip_ranges() {
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "151.101.1.69",
            "2606:4700::1111",
            "2a00:1450::1",
        ];
        for ip in public {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be public");
        }
        let private = [
            "10.0.0.1",
            "100.127.255.255",
            "127.8.8.8",
            "172.31.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "ff02::1",
            "fc00::1",
            "2001:db8::1",
            "100::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
        ];
        for ip in private {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        // Edges just outside the private /12 and the CGNAT /10 stay public.
        assert!(is_public_ip("172.32.0.1".parse().unwrap()));
        assert!(is_public_ip("100.128.0.1".parse().unwrap()));
        assert!(is_public_ip("64:ff9b::808:808".parse().unwrap()));
    }

    fn no_cap(_: Option<&str>) -> BodyCap {
        BodyCap::Refuse(usize::MAX)
    }

    async fn fetch_test(
        url: String,
        cap: &(dyn Fn(Option<&str>) -> BodyCap + Sync),
    ) -> Result<FetchedBody, PublicFetchError> {
        fetch_with_policy(
            Url::parse(&url).unwrap(),
            cap,
            FetchPolicy::PublicOrLoopbackForTests,
        )
        .await
    }

    #[tokio::test]
    async fn production_policy_refuses_the_loopback_mock() {
        let server = MockServer::start().await;
        let err = fetch_with_policy(
            Url::parse(&format!("{}/a", server.uri())).unwrap(),
            &no_cap,
            FetchPolicy::PublicInternet,
        )
        .await
        .unwrap_err();
        assert_eq!(err, PublicFetchError::InvalidUrl);
    }

    #[tokio::test]
    async fn follows_redirects_within_the_limit() {
        let server = MockServer::start().await;
        for (from, to) in [("/a", "/b"), ("/b", "/c"), ("/c", "/d")] {
            Mock::given(method("GET"))
                .and(path(from))
                .respond_with(ResponseTemplate::new(302).insert_header("Location", to))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/d"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"ok".to_vec(), "text/HTML; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let body = fetch_test(format!("{}/a", server.uri()), &no_cap)
            .await
            .unwrap();
        assert_eq!(body.bytes, b"ok");
        assert_eq!(body.final_url.path(), "/d");
        assert_eq!(
            body.content_type.as_deref(),
            Some("text/html; charset=utf-8")
        );
    }

    #[tokio::test]
    async fn stops_after_too_many_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(301).insert_header("Location", "/loop"))
            .expect(MAX_REDIRECTS as u64 + 1)
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/loop", server.uri()), &no_cap)
            .await
            .unwrap_err();
        assert_eq!(err, PublicFetchError::TooManyRedirects);
    }

    #[tokio::test]
    async fn rechecks_every_redirect_target() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/to-lan"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "https://192.168.1.1/admin"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/to-http"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "http://example.org/a.jpg"),
            )
            .mount(&server)
            .await;

        let err = fetch_test(format!("{}/to-lan", server.uri()), &no_cap)
            .await
            .unwrap_err();
        assert_eq!(err, PublicFetchError::BlockedAddress);
        let err = fetch_test(format!("{}/to-http", server.uri()), &no_cap)
            .await
            .unwrap_err();
        assert_eq!(err, PublicFetchError::InvalidUrl);
    }

    #[tokio::test]
    async fn a_redirect_without_target_is_an_http_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(300))
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/choices", server.uri()), &no_cap)
            .await
            .unwrap_err();
        assert_eq!(err, PublicFetchError::HttpStatus(300));
    }

    #[test]
    fn a_redirect_through_http_is_followed_over_https() {
        let here = Url::parse("https://www.example.org/livre/42").unwrap();
        let next = redirect_target(
            &here,
            Some("http://img.example.org/couv.jpg"),
            301,
            FetchPolicy::PublicInternet,
        )
        .unwrap();
        assert_eq!(next.as_str(), "https://img.example.org/couv.jpg");
        assert_eq!(
            redirect_target(
                &here,
                Some("ftp://x.org/a"),
                301,
                FetchPolicy::PublicInternet
            ),
            Err(PublicFetchError::InvalidUrl)
        );
        assert_eq!(
            redirect_target(&here, None, 304, FetchPolicy::PublicInternet),
            Err(PublicFetchError::HttpStatus(304))
        );
    }

    #[test]
    fn upgrades_http_links_to_https() {
        let up = upgrade_to_https(Url::parse("http://example.org/a.jpg").unwrap()).unwrap();
        assert_eq!(up.as_str(), "https://example.org/a.jpg");
        assert!(upgrade_to_https(Url::parse("http://example.org:8080/a.jpg").unwrap()).is_none());
        assert!(upgrade_to_https(Url::parse("ftp://example.org/a.jpg").unwrap()).is_none());
    }

    #[tokio::test]
    async fn truncates_a_body_that_may_be_cut() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![b'x'; 5000], "text/html"))
            .mount(&server)
            .await;
        let body = fetch_test(format!("{}/page", server.uri()), &|_| {
            BodyCap::Truncate(1000)
        })
        .await
        .unwrap();
        assert_eq!(body.bytes.len(), 1000);
    }

    /// A server that drops the first connection without answering, then
    /// serves `body` as an image. Returns its base URL.
    async fn flaky_server(body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut first = true;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                if first {
                    first = false;
                    continue; // dropped: the client sees an incomplete message
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn retries_once_when_the_server_drops_the_connection() {
        // Seen live on a publisher's CDN: about one request in four closed
        // before any answer. A GET is safe to send again.
        let base = flaky_server(b"cover").await;
        let body = fetch_test(format!("{base}/couv.jpg"), &no_cap)
            .await
            .unwrap();
        assert_eq!(body.bytes, b"cover");
    }

    #[tokio::test]
    async fn reports_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = fetch_test(format!("{}/missing", server.uri()), &no_cap)
            .await
            .unwrap_err();
        assert_eq!(err, PublicFetchError::HttpStatus(404));
    }

    #[tokio::test]
    async fn caps_the_body_by_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(vec![b'x'; 2000], "text/html"))
            .mount(&server)
            .await;
        let cap = |ct: Option<&str>| {
            if ct.is_some_and(|c| c.starts_with("text/html")) {
                BodyCap::Refuse(1000)
            } else {
                BodyCap::Refuse(usize::MAX)
            }
        };
        let err = fetch_test(format!("{}/page", server.uri()), &cap)
            .await
            .unwrap_err();
        assert_eq!(err, PublicFetchError::TooLarge);
        assert!(
            fetch_test(format!("{}/page", server.uri()), &no_cap)
                .await
                .is_ok()
        );
    }
}
