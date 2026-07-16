//! Shared helpers for the P2P peer API: URL validation, HTTP client, peer registration and approval checks.

use crate::models::peer;
use axum::http::StatusCode;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};
use url::Url;

/// Validate URL to prevent SSRF (OWASP A10).
///
/// Blocks:
/// - Non-HTTP/HTTPS schemes (file://, ftp://, javascript:, etc.)
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-local (169.254.0.0/16, fe80::/10) - includes AWS metadata 169.254.169.254
/// - Multicast (224.0.0.0/4, ff00::/8)
/// - Unspecified (0.0.0.0, ::)
/// - Broadcast (255.255.255.255)
/// - "localhost" hostname
///
/// Allows:
/// - Private networks (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) for P2P LAN use
pub fn validate_url(url_str: &str) -> Result<String, String> {
    let url = Url::parse(url_str).map_err(|_| "Invalid URL format".to_string())?;

    // 1. Check Scheme
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Only HTTP/HTTPS schemes allowed".to_string());
    }

    // 2. Check Host
    match url.host() {
        Some(url::Host::Domain("localhost")) => {
            return Err("Localhost access is blocked".to_string());
        }
        Some(url::Host::Ipv4(ip)) => {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            // Link-local: 169.254.0.0/16 (includes AWS metadata endpoint 169.254.169.254)
            let octets = ip.octets();
            if octets[0] == 169 && octets[1] == 254 {
                return Err("Link-local addresses blocked".to_string());
            }
            if ip.is_multicast() {
                return Err("Multicast addresses blocked".to_string());
            }
            if ip.is_unspecified() {
                return Err("Unspecified address blocked".to_string());
            }
            // Broadcast: 255.255.255.255
            if octets == [255, 255, 255, 255] {
                return Err("Broadcast address blocked".to_string());
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            if ip.is_multicast() {
                return Err("Multicast addresses blocked".to_string());
            }
            if ip.is_unspecified() {
                return Err("Unspecified address blocked".to_string());
            }
            // IPv6 link-local: fe80::/10
            let segments = ip.segments();
            if (segments[0] & 0xffc0) == 0xfe80 {
                return Err("Link-local addresses blocked".to_string());
            }
        }
        None => {
            return Err("URL must have a host".to_string());
        }
        _ => {}
    }

    Ok(url.to_string())
}

/// Look up a peer by URL, tolerating the trailing-slash discrepancy between
/// how URLs are stored at pairing time (raw, un-normalized) and how they are
/// presented by callers (sometimes slash, sometimes not).
async fn find_peer_by_url(
    db: &DatabaseConnection,
    url: &str,
) -> Result<Option<peer::Model>, StatusCode> {
    let trimmed = url.trim_end_matches('/').to_string();
    let with_slash = format!("{trimmed}/");
    peer::Entity::find()
        .filter(
            Condition::any()
                .add(peer::Column::Url.eq(&trimmed))
                .add(peer::Column::Url.eq(&with_slash)),
        )
        .one(db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Ensure `url` refers to a peer already registered in the local DB — strict.
///
/// SSRF defense for peer-proxy endpoints: validate_url() blocks loopback and
/// link-local, but RFC1918 ranges are allowed for LAN peer discovery. Without
/// this second check, a caller could proxy through cover_proxy to probe any
/// service reachable on the local network (router admin, NAS, printers).
/// Requiring a DB match constrains peer_url to URLs vetted by the user via
/// pairing.
///
/// Use this variant for endpoints that have no legitimate "unsaved mDNS peer"
/// flow (e.g. cover_proxy, which fetches binary payloads from a URL that must
/// be user-approved). For endpoints with a legitimate mDNS fallback (browse a
/// neighbor's library before pairing), use `ensure_registered_peer_or_mdns`.
pub async fn ensure_registered_peer(
    db: &DatabaseConnection,
    url: &str,
) -> Result<peer::Model, StatusCode> {
    match ensure_registered_peer_or_mdns(db, url, false).await? {
        Some(p) => Ok(p),
        None => {
            // Unreachable: allow_unregistered_lan=false forces Err on absent.
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// Ensure `url` refers to a peer already registered, with optional mDNS
/// fallback for endpoints that must accept unsaved LAN peers (ADR-026).
///
/// Returns:
/// - `Ok(Some(peer))` when the URL matches a registered peer row.
/// - `Ok(None)` when the URL is unknown AND `allow_unregistered_lan=true`.
///   A `warn!(target = "ssrf:mdns", ...)` entry is emitted so the audit
///   trail captures every fallback traversal.
/// - `Err(StatusCode::FORBIDDEN)` when the URL is unknown AND
///   `allow_unregistered_lan=false` (strict mode, matches the original
///   `ensure_registered_peer` contract).
///
/// Callers receiving `Ok(None)` MUST treat the peer as untrusted: skip
/// outgoing-request tracking, cache enrichment, and any operation that
/// relies on a stable peer identity.
pub async fn ensure_registered_peer_or_mdns(
    db: &DatabaseConnection,
    url: &str,
    allow_unregistered_lan: bool,
) -> Result<Option<peer::Model>, StatusCode> {
    match find_peer_by_url(db, url).await? {
        Some(p) => Ok(Some(p)),
        None => {
            let safe: String = url.chars().take(128).collect();
            if allow_unregistered_lan {
                tracing::warn!(
                    target: "ssrf:mdns",
                    "peer-proxy fallback: unregistered URL allowed via mDNS path (url={safe})"
                );
                Ok(None)
            } else {
                tracing::warn!(
                    target: "ssrf",
                    "peer-proxy rejected: peer not registered (url={safe})"
                );
                Err(StatusCode::FORBIDDEN)
            }
        }
    }
}

/// Create a safe HTTP client with restricted redirects and timeouts
pub(crate) fn get_safe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()) // Disable redirects to prevent bypass
        .build()
        .unwrap_or_default()
}

/// Translate localhost URLs to Docker service names for inter-container communication
/// Examples:
/// - http://localhost:8001 -> http://bibliogenius-a:8000
/// - http://localhost:8002 -> http://bibliogenius-b:8000
pub(crate) fn translate_url_for_docker(url: &str) -> String {
    if url.contains("localhost:8001") {
        url.replace("localhost:8001", "bibliogenius-a:8000")
    } else if url.contains("localhost:8002") {
        url.replace("localhost:8002", "bibliogenius-b:8000")
    } else {
        url.to_string()
    }
}

/// Check if the `connection_validation` module is enabled in installation profile
pub(crate) async fn is_connection_validation_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile;

    if let Ok(Some(profile)) = installation_profile::Entity::find().one(db).await {
        return profile.enabled_modules.contains("connection_validation");
    }
    false
}

/// Check if `auto_approve_loans` module is enabled in installation profile
pub(crate) async fn is_auto_approve_loans_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile;

    if let Ok(Some(profile)) = installation_profile::Entity::find().one(db).await {
        return profile.enabled_modules.contains("auto_approve_loans");
    }
    false
}

/// Check if a specific peer is approved for access.
/// Returns true if connection_validation is disabled OR if the peer has connection_status == "accepted".
pub(crate) async fn is_peer_approved(db: &DatabaseConnection, peer: &peer::Model) -> bool {
    if !is_connection_validation_enabled(db).await {
        return true;
    }
    peer.connection_status == "accepted"
}
