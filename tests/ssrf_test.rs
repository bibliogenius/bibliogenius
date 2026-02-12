//! SSRF Protection Tests — validate_url()
//!
//! Covers: B11.1 Peer URL Validation (TNR)
//! Tests the URL validation function that prevents Server-Side Request Forgery
//! when connecting to P2P peers.

use rust_lib_app::api::peer::validate_url;

#[test]
fn test_valid_http_url_accepted() {
    let result = validate_url("http://192.168.1.100:8000");
    assert!(result.is_ok(), "Valid LAN HTTP URL should be accepted");
}

#[test]
fn test_valid_https_url_accepted() {
    let result = validate_url("https://192.168.1.100:8000");
    assert!(result.is_ok(), "Valid LAN HTTPS URL should be accepted");
}

#[test]
fn test_valid_private_network_10_accepted() {
    let result = validate_url("http://10.0.0.1:8000");
    assert!(
        result.is_ok(),
        "10.x.x.x private network should be accepted"
    );
}

#[test]
fn test_valid_private_network_172_accepted() {
    let result = validate_url("http://172.16.0.1:8000");
    assert!(
        result.is_ok(),
        "172.16.x.x private network should be accepted"
    );
}

#[test]
fn test_localhost_hostname_blocked() {
    let result = validate_url("http://localhost:8000");
    assert!(result.is_err(), "localhost hostname must be blocked");
    assert!(
        result.unwrap_err().contains("Localhost"),
        "Error should mention localhost"
    );
}

#[test]
fn test_loopback_127_0_0_1_blocked() {
    let result = validate_url("http://127.0.0.1:8000");
    assert!(result.is_err(), "127.0.0.1 loopback must be blocked");
    assert!(
        result.unwrap_err().contains("Loopback"),
        "Error should mention loopback"
    );
}

#[test]
fn test_loopback_127_0_0_2_blocked() {
    let result = validate_url("http://127.0.0.2:8000");
    assert!(result.is_err(), "127.0.0.2 loopback must be blocked");
}

#[test]
fn test_ipv6_loopback_blocked() {
    let result = validate_url("http://[::1]:8000");
    assert!(result.is_err(), "IPv6 loopback ::1 must be blocked");
}

#[test]
fn test_ftp_scheme_blocked() {
    let result = validate_url("ftp://192.168.1.100/file");
    assert!(result.is_err(), "FTP scheme must be blocked");
    assert!(
        result.unwrap_err().contains("HTTP/HTTPS"),
        "Error should mention allowed schemes"
    );
}

#[test]
fn test_file_scheme_blocked() {
    let result = validate_url("file:///etc/passwd");
    assert!(result.is_err(), "file:// scheme must be blocked");
}

#[test]
fn test_javascript_scheme_blocked() {
    let result = validate_url("javascript:alert(1)");
    assert!(result.is_err(), "javascript: scheme must be blocked");
}

#[test]
fn test_invalid_url_format_rejected() {
    let result = validate_url("not-a-valid-url");
    assert!(result.is_err(), "Invalid URL format must be rejected");
    assert!(
        result.unwrap_err().contains("Invalid URL"),
        "Error should mention invalid format"
    );
}

#[test]
fn test_empty_string_rejected() {
    let result = validate_url("");
    assert!(result.is_err(), "Empty string must be rejected");
}

#[test]
fn test_url_with_path_accepted() {
    let result = validate_url("http://192.168.1.100:8000/api/books");
    assert!(result.is_ok(), "URL with path should be accepted");
}

#[test]
fn test_url_without_port_accepted() {
    let result = validate_url("http://192.168.1.100");
    assert!(result.is_ok(), "URL without port should be accepted");
}
