//! Hub URL comparison helpers.
//!
//! Two sites in the codebase wipe `hub_directory_config` when the hub URL
//! changes (`api/peer.rs::apply_relay_setup` and `api/frb.rs::init_backend`).
//! Both must agree on what "changed" means, otherwise a trailing slash
//! alone would burn the stored `write_token` + `recovery_code` and lock
//! the client into a 401 loop.

/// Return `true` when two hub URLs point to different hubs, ignoring a
/// trailing slash. Kept deliberately strict on scheme, host, and port —
/// any other difference means we cannot assume the old `write_token` is
/// valid on the new hub.
pub(crate) fn hub_urls_differ(prev: &str, new: &str) -> bool {
    prev.trim_end_matches('/') != new.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_is_not_a_difference() {
        assert!(!hub_urls_differ(
            "https://hub.example.org",
            "https://hub.example.org/"
        ));
        assert!(!hub_urls_differ(
            "https://hub.example.org/",
            "https://hub.example.org"
        ));
        assert!(!hub_urls_differ(
            "https://hub.example.org",
            "https://hub.example.org"
        ));
    }

    #[test]
    fn different_hosts_are_different() {
        assert!(hub_urls_differ(
            "https://hub.example.org",
            "https://hub-dev.example.org"
        ));
    }

    #[test]
    fn different_schemes_are_different() {
        assert!(hub_urls_differ(
            "http://hub.example.org",
            "https://hub.example.org"
        ));
    }

    #[test]
    fn different_ports_are_different() {
        assert!(hub_urls_differ(
            "https://hub.example.org",
            "https://hub.example.org:8443"
        ));
    }
}
