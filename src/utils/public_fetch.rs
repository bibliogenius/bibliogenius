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
//!   connection is pinned to the checked address, so a DNS answer that changes
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a public fetch failed. Callers map these to user-facing messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicFetchError {
    /// Not an https URL we accept (bad syntax, other scheme, credentials).
    InvalidUrl,
    /// The URL points at, or resolves to, a non-public address or port.
    BlockedAddress,
    /// DNS failure, connection failure or timeout.
    Network,
    /// More than [`MAX_REDIRECTS`] redirects, or a redirect without target.
    TooManyRedirects,
    /// The server answered with a non-success status.
    HttpStatus(u16),
    /// The body exceeds the cap the caller set for its content type.
    TooLarge,
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
    let blocked = s[..6] == [0, 0, 0, 0, 0, 0] // ::, ::1, IPv4-compatible
        || (s[0] & 0xff00) == 0xff00 // multicast
        || (s[0] & 0xfe00) == 0xfc00 // unique local
        || (s[0] & 0xffc0) == 0xfe80 // link-local
        || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0]); // discard prefix
    !blocked
}

/// Resolve the URL's host and return an address every resolved answer agrees
/// is public. IP literals were already judged by the policy check. Refusing
/// when ANY answer is non-public (rather than picking a public one) keeps a
/// name with mixed answers from being half-trusted.
async fn resolve_checked(url: &Url) -> Result<SocketAddr, PublicFetchError> {
    let port = url
        .port_or_known_default()
        .ok_or(PublicFetchError::InvalidUrl)?;
    let domain = match url.host() {
        Some(Host::Ipv4(ip)) => return Ok(SocketAddr::new(IpAddr::V4(ip), port)),
        Some(Host::Ipv6(ip)) => return Ok(SocketAddr::new(IpAddr::V6(ip), port)),
        Some(Host::Domain(d)) => d.to_owned(),
        None => return Err(PublicFetchError::InvalidUrl),
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain.as_str(), port))
        .await
        .map_err(|_| PublicFetchError::Network)?
        .collect();
    let Some(first) = addrs.first().copied() else {
        return Err(PublicFetchError::Network);
    };
    if addrs.iter().any(|a| !is_public_ip(a.ip())) {
        tracing::warn!(target: "ssrf", "public fetch refused: {domain} resolves to a non-public address");
        return Err(PublicFetchError::BlockedAddress);
    }
    Ok(first)
}

/// Fetch `start` (checked with [`parse_public_https_url`] by the caller) under
/// `policy`, following redirects by hand.
///
/// `max_bytes` picks the body cap from the response's content type (lowercased,
/// `None` when absent), so an HTML page and an image can be held to different
/// limits by one call. The policy is a parameter so tests can point the loop at
/// a loopback mock server.
pub(crate) async fn fetch_with_policy(
    start: Url,
    max_bytes: &(dyn Fn(Option<&str>) -> usize + Sync),
    policy: FetchPolicy,
) -> Result<FetchedBody, PublicFetchError> {
    let mut url = start;
    for _hop in 0..=MAX_REDIRECTS {
        policy.check(&url)?;
        let addr = resolve_checked(&url).await?;

        let mut builder = reqwest::Client::builder()
            .user_agent(API_USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if let Some(Host::Domain(domain)) = url.host() {
            // Pin the connection to the address checked above.
            builder = builder.resolve(domain, addr);
        }
        let client = builder.build().map_err(|_| PublicFetchError::Network)?;

        let mut resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| PublicFetchError::Network)?;
        let status = resp.status();

        if status.is_redirection() {
            let next = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| url.join(loc).ok())
                .ok_or(PublicFetchError::TooManyRedirects)?;
            url = next;
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
        let cap = max_bytes(content_type.as_deref());
        if resp.content_length().is_some_and(|len| len > cap as u64) {
            return Err(PublicFetchError::TooLarge);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| PublicFetchError::Network)? {
            if bytes.len() + chunk.len() > cap {
                return Err(PublicFetchError::TooLarge);
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

    fn no_cap(_: Option<&str>) -> usize {
        usize::MAX
    }

    async fn fetch_test(
        url: String,
        cap: &(dyn Fn(Option<&str>) -> usize + Sync),
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
                1000
            } else {
                usize::MAX
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
