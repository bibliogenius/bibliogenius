use std::net::IpAddr;
use url::Url;

/// Port range used by BiblioGenius Axum servers on LAN.
const PORT_RANGE: std::ops::RangeInclusive<u16> = 8000..=8010;

/// Timeout per port probe (fast on LAN).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Try to find a peer on a different port when the stored URL is unreachable.
///
/// Extracts the host from `stored_url`, then probes ports 8000-8010
/// (skipping the stored port) looking for a live BiblioGenius `/api/config`.
///
/// Security: only probes the same host (already validated by `validate_url`
/// in the caller), restricted to ports 8000-8010, and verifies the response
/// contains a `library_name` field to confirm it is a BiblioGenius instance.
///
/// Returns the new base URL (e.g. `http://192.168.1.53:8006`) if found,
/// or `None` if the peer is unreachable on all ports.
pub async fn try_discover_peer_port(stored_url: &str, client: &reqwest::Client) -> Option<String> {
    let parsed = Url::parse(stored_url).ok()?;
    let host = parsed.host_str()?;

    // Safety: reject loopback and non-private hosts
    if !is_private_lan_ip(host) {
        tracing::debug!("Port discovery: skipping non-LAN host {}", host);
        return None;
    }

    let stored_port = parsed.port().unwrap_or(8000);

    for port in PORT_RANGE {
        if port == stored_port {
            continue;
        }
        let test_url = format!("http://{}:{}", host, port);
        let probe = format!("{}/api/config", test_url);
        match client.get(&probe).timeout(PROBE_TIMEOUT).send().await {
            Ok(res) if res.status().is_success() => {
                // Verify the response is a BiblioGenius config (not another service)
                if let Ok(body) = res.json::<serde_json::Value>().await
                    && body.get("library_name").and_then(|v| v.as_str()).is_some()
                {
                    tracing::info!(
                        "Port discovery: peer found at {} (stored: {})",
                        test_url,
                        stored_url
                    );
                    return Some(test_url);
                }
            }
            _ => continue,
        }
    }
    None
}

/// Check if a host string is a private LAN IPv4 address.
/// Rejects loopback (127.x), link-local (169.254.x), and public IPs.
fn is_private_lan_ip(host: &str) -> bool {
    let ip: IpAddr = match host.parse() {
        Ok(ip) => ip,
        Err(_) => return false, // Hostnames are not scanned
    };
    match ip {
        IpAddr::V4(v4) => {
            // RFC 1918 private ranges only
            (v4.octets()[0] == 10)
                || (v4.octets()[0] == 172 && (16..=31).contains(&v4.octets()[1]))
                || (v4.octets()[0] == 192 && v4.octets()[1] == 168)
        }
        IpAddr::V6(_) => false, // No port scanning on IPv6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_range() {
        assert!(PORT_RANGE.contains(&8000));
        assert!(PORT_RANGE.contains(&8010));
        assert!(!PORT_RANGE.contains(&8011));
    }

    #[test]
    fn test_private_lan_ip() {
        assert!(is_private_lan_ip("192.168.1.53"));
        assert!(is_private_lan_ip("10.0.0.1"));
        assert!(is_private_lan_ip("172.16.0.1"));
        assert!(is_private_lan_ip("172.31.255.255"));
        assert!(!is_private_lan_ip("127.0.0.1")); // loopback
        assert!(!is_private_lan_ip("169.254.1.1")); // link-local
        assert!(!is_private_lan_ip("8.8.8.8")); // public
        assert!(!is_private_lan_ip("172.32.0.1")); // outside 172.16-31 range
        assert!(!is_private_lan_ip("my-host.local")); // hostname
    }

    #[test]
    fn test_parse_stored_url() {
        let url = Url::parse("http://192.168.1.53:8000").unwrap();
        assert_eq!(url.host_str(), Some("192.168.1.53"));
        assert_eq!(url.port(), Some(8000));
    }
}
