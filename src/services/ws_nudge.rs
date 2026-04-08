//! WebSocket nudge listener for instant relay notifications.
//!
//! Connects to the Hub's WS sidecar and listens for "mailbox_nudge" signals.
//! On nudge, triggers an immediate `poll_once()` cycle instead of waiting for
//! the next polling interval. The existing 20s polling loop remains as fallback.
//!
//! See ADR-017 for architecture details.

use std::time::Duration;

use futures::StreamExt;
use tokio_tungstenite::tungstenite;

use crate::api::relay::get_my_relay_config;
use crate::infrastructure::AppState;
use crate::services::nudge_events::NudgeSource;
use crate::services::relay_poller::poll_once;

/// Maximum reconnection backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Initial reconnection delay.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Delay before retrying when no relay config is available.
const NO_CONFIG_RETRY: Duration = Duration::from_secs(60);

/// Start the WebSocket nudge listener.
///
/// Runs forever. Connects to the Hub's WS sidecar, listens for nudge signals,
/// and triggers `poll_once()` on each nudge. Reconnects with exponential backoff
/// on disconnection.
///
/// Spawn this as a background task alongside the existing polling loop.
pub async fn start_ws_nudge(state: AppState) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        // Load relay config (may not be set up yet).
        let config = match get_my_relay_config(state.db()).await {
            Some(c) => c,
            None => {
                tracing::debug!("WS nudge: no relay config, retrying in 60s");
                tokio::time::sleep(NO_CONFIG_RETRY).await;
                continue;
            }
        };

        // Derive WS URL from relay_url.
        // relay_url is like "https://hub.bibliogenius.org" or "https://hub-dev.bibliogenius.org"
        let ws_url = match build_ws_url(&config.relay_url, &config.mailbox_uuid) {
            Some(url) => url,
            None => {
                tracing::warn!(
                    "WS nudge: cannot build WS URL from relay_url: {}",
                    config.relay_url
                );
                tokio::time::sleep(NO_CONFIG_RETRY).await;
                continue;
            }
        };

        tracing::info!("WS nudge: connecting to {ws_url}");

        match connect_and_listen(&state, &ws_url, &config.read_token).await {
            Ok(()) => {
                // Clean disconnect (server closed). Reconnect immediately.
                tracing::info!("WS nudge: connection closed, reconnecting");
                backoff = INITIAL_BACKOFF;
            }
            Err(e) => {
                tracing::warn!("WS nudge: connection error: {e}, retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Build the WebSocket URL from the relay URL.
///
/// Converts "https://hub.example.org" to "wss://hub.example.org/ws?mailbox_id={uuid}"
/// and "http://..." to "ws://...".
fn build_ws_url(relay_url: &str, mailbox_uuid: &str) -> Option<String> {
    let ws_base = if relay_url.starts_with("https://") {
        relay_url.replacen("https://", "wss://", 1)
    } else if relay_url.starts_with("http://") {
        relay_url.replacen("http://", "ws://", 1)
    } else {
        return None;
    };

    // Strip trailing slash if present.
    let base = ws_base.trim_end_matches('/');
    Some(format!("{base}/ws?mailbox_id={mailbox_uuid}"))
}

/// Connect to the WS sidecar and listen for nudges.
///
/// Returns `Ok(())` on clean disconnect, `Err` on connection or protocol error.
async fn connect_and_listen(
    state: &AppState,
    ws_url: &str,
    read_token: &str,
) -> Result<(), String> {
    // Build the upgrade request with Authorization header (no token in URL).
    let request = tungstenite::http::Request::builder()
        .uri(ws_url)
        .header("Authorization", format!("Bearer {read_token}"))
        .header("Host", extract_host(ws_url).unwrap_or("localhost"))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .map_err(|e| format!("failed to build request: {e}"))?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("connection failed: {e}"))?;

    tracing::info!("WS nudge: connected");

    let (_write, mut read) = ws_stream.split();

    while let Some(msg_result) = read.next().await {
        match msg_result {
            Ok(tungstenite::Message::Text(text)) => {
                handle_nudge_message(state, &text).await;
            }
            Ok(tungstenite::Message::Ping(_)) => {
                // Pong is sent automatically by tungstenite.
            }
            Ok(tungstenite::Message::Close(_)) => {
                tracing::debug!("WS nudge: server sent close frame");
                return Ok(());
            }
            Ok(_) => {
                // Ignore binary, pong, frame messages.
            }
            Err(e) => {
                return Err(format!("read error: {e}"));
            }
        }
    }

    // Stream ended (server dropped connection).
    Ok(())
}

/// Process a nudge text message. Triggers immediate poll if valid.
async fn handle_nudge_message(state: &AppState, text: &str) {
    // Parse the nudge JSON.
    let json: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("WS nudge: invalid JSON: {e}");
            return;
        }
    };

    // Verify it is a mailbox_nudge.
    if json.get("type").and_then(|v| v.as_str()) != Some("mailbox_nudge") {
        tracing::debug!("WS nudge: ignoring non-nudge message: {text}");
        return;
    }

    tracing::info!("WS nudge: received nudge, triggering immediate poll");
    if let Err(e) = poll_once(state, NudgeSource::WebSocket).await {
        tracing::warn!("WS nudge: poll_once failed: {e}");
    }
}

/// Extract the host portion from a URL for the Host header.
fn extract_host(url: &str) -> Option<&str> {
    // Skip scheme ("wss://" or "ws://").
    let after_scheme = url.split("://").nth(1)?;
    // Take everything before the first "/" or "?".
    let host = after_scheme.split('/').next()?;
    let host = host.split('?').next()?;
    Some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ws_url_https() {
        let url = build_ws_url("https://hub.bibliogenius.org", "abc-123").unwrap();
        assert_eq!(url, "wss://hub.bibliogenius.org/ws?mailbox_id=abc-123");
    }

    #[test]
    fn build_ws_url_http() {
        let url = build_ws_url("http://localhost:8080", "abc-123").unwrap();
        assert_eq!(url, "ws://localhost:8080/ws?mailbox_id=abc-123");
    }

    #[test]
    fn build_ws_url_trailing_slash() {
        let url = build_ws_url("https://hub.example.org/", "uuid-here").unwrap();
        assert_eq!(url, "wss://hub.example.org/ws?mailbox_id=uuid-here");
    }

    #[test]
    fn build_ws_url_invalid_scheme() {
        assert!(build_ws_url("ftp://example.org", "uuid").is_none());
    }

    #[test]
    fn extract_host_simple() {
        assert_eq!(
            extract_host("wss://hub.example.org/ws"),
            Some("hub.example.org")
        );
    }

    #[test]
    fn extract_host_with_port() {
        assert_eq!(
            extract_host("ws://localhost:9091/ws?foo=bar"),
            Some("localhost:9091")
        );
    }
}
