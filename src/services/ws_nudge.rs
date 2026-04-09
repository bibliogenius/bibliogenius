//! WebSocket nudge listener for instant relay notifications.
//!
//! Connects to the Hub's WS sidecar and listens for "mailbox_nudge" signals.
//! On nudge, triggers an immediate poll cycle (via `poll_once_wait()`) instead
//! of waiting for the next polling interval. The existing 20s polling loop
//! remains as fallback.
//!
//! See ADR-017 for architecture details.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite;

use crate::api::relay::get_my_relay_config;
use crate::infrastructure::AppState;
use crate::services::nudge_events::NudgeSource;
use crate::services::relay_poller::poll_once_wait;

/// Maximum reconnection backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Initial reconnection delay.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Delay before retrying when no relay config is available.
const NO_CONFIG_RETRY: Duration = Duration::from_secs(60);

/// Start the WebSocket nudge listener.
///
/// Runs forever. Connects to the Hub's WS sidecar, listens for nudge signals,
/// and triggers `poll_once_wait()` on each nudge. Reconnects with exponential backoff
/// on disconnection.
///
/// A single background poll worker is spawned once and shared across all WS
/// connections. The WS message loop is non-blocking: nudges signal the worker
/// via a `Notify` and return immediately. Multiple nudges arriving while a poll
/// is in progress coalesce into at most one extra poll cycle (dirty flag
/// semantics implemented via `Notify`'s single-token storage).
///
/// Spawn this as a background task alongside the existing polling loop.
pub async fn start_ws_nudge(state: AppState) {
    let mut backoff = INITIAL_BACKOFF;

    // One poll worker for the lifetime of this task.
    // The Notify acts as a coalescing dirty flag: notify_one() stores at most
    // one pending token, so concurrent nudges result in at most one extra poll.
    //
    // The worker is wrapped in a supervision loop: if poll_once() panics and
    // kills the inner task, the supervisor restarts it automatically. Any
    // Notify tokens stored during the crash window are preserved because the
    // Arc<Notify> outlives the inner task (the supervisor holds its own clone).
    let poll_notify: Arc<Notify> = Arc::new(Notify::new());
    {
        let notify = poll_notify.clone();
        let worker_state = state.clone();
        tokio::spawn(async move {
            loop {
                let n = notify.clone();
                let s = worker_state.clone();
                if let Err(e) = tokio::spawn(poll_worker(s, n)).await {
                    tracing::error!(
                        "WS nudge: poll_worker terminated unexpectedly ({e}), restarting"
                    );
                }
                // Normal path: poll_worker loops forever and never returns Ok(()).
                // Reaching here always means an unexpected termination.
            }
        });
    }

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

        match connect_and_listen(&ws_url, &config.read_token, &poll_notify).await {
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

/// Background poll worker driven by nudge notifications.
///
/// Waits for a signal via `notify`, then calls `poll_once_wait()`. Unlike
/// `poll_once()`, `poll_once_wait()` blocks until the relay poll lock is free
/// rather than skipping — so WS nudges are never silently dropped when the
/// 2-second peer.rs loop or another caller holds the lock.
///
/// If another nudge arrives while a poll is in progress, `notify_one()` stores
/// a single pending token (dirty-flag semantics). When the current poll finishes,
/// `notified().await` returns immediately for that token, triggering one extra
/// poll cycle: at most one extra poll per concurrent burst of nudges.
async fn poll_worker(state: AppState, notify: Arc<Notify>) {
    loop {
        // Block until a nudge signals us. Returns immediately if a token was
        // stored by a nudge that arrived while we were running poll_once_wait().
        notify.notified().await;

        tracing::info!("WS nudge: poll worker triggered");
        // poll_once_wait() blocks until the lock is free (lock().await), then
        // runs the full poll cycle. This guarantees every WS nudge results in
        // a poll, even if another caller holds the lock when the nudge arrives.
        if let Err(e) = poll_once_wait(&state, NudgeSource::WebSocket).await {
            tracing::warn!("WS nudge: poll_once_wait failed: {e}");
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
/// The `notify` reference is passed to `handle_nudge_message` so each nudge
/// signals the poll worker without blocking this loop.
async fn connect_and_listen(
    ws_url: &str,
    read_token: &str,
    notify: &Arc<Notify>,
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
                handle_nudge_message(notify, &text);
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

/// Process a nudge text message. Signals the poll worker if valid.
///
/// This function is synchronous and non-blocking: it parses the JSON, validates
/// the message type, then calls `notify.notify_one()`. The poll worker handles
/// the actual `poll_once()` call asynchronously, so the WS message loop is
/// never blocked waiting for a poll to complete.
fn handle_nudge_message(notify: &Arc<Notify>, text: &str) {
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

    tracing::info!("WS nudge: received nudge, signaling poll worker");
    // Signal the poll worker. If a poll is already in progress, notify_one()
    // stores one pending token (the dirty flag), triggering a re-poll when
    // the current poll finishes. Multiple nudges during one poll coalesce
    // into at most one extra poll cycle.
    notify.notify_one();
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    // --- URL / host helpers (unchanged) ---

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

    // --- Poll worker coalescing behavior ---

    /// Tokio's Notify stores at most one pending token regardless of how many
    /// times notify_one() is called. This validates the dirty-flag semantics:
    /// N concurrent nudges result in exactly one extra poll cycle.
    #[tokio::test]
    async fn notify_coalesces_concurrent_nudges() {
        let notify = Notify::new();

        // Fire 3 nudges before any consumer is waiting.
        notify.notify_one();
        notify.notify_one();
        notify.notify_one();

        // Only one token is stored - first notified() returns immediately.
        tokio::time::timeout(Duration::from_millis(5), notify.notified())
            .await
            .expect("first notified() should return immediately from stored token");

        // No further tokens: second notified() should time out.
        let pending = tokio::time::timeout(Duration::from_millis(5), notify.notified()).await;
        assert!(
            pending.is_err(),
            "no further tokens should be pending after consuming one"
        );
    }

    /// A nudge that arrives while a poll is in progress stores a token.
    /// The worker runs a second poll immediately after the first finishes.
    #[tokio::test]
    async fn nudge_during_poll_triggers_one_extra_cycle() {
        let notify = Arc::new(Notify::new());
        let poll_count = Arc::new(AtomicU32::new(0));

        let n = notify.clone();
        let c = poll_count.clone();

        // Simulate the poll_worker loop with a mock poll (50 ms sleep).
        let worker = tokio::spawn(async move {
            for _ in 0..2 {
                n.notified().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Initial nudge at t=0.
        notify.notify_one();

        // Second nudge at t=20ms - arrives while the 50ms mock poll is running.
        tokio::time::sleep(Duration::from_millis(20)).await;
        notify.notify_one();

        // Wait for both poll cycles to complete (generous timeout).
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("worker should finish within 1s")
            .expect("worker task should not panic");

        assert_eq!(
            poll_count.load(Ordering::SeqCst),
            2,
            "expected exactly 2 poll cycles: 1 initial + 1 for nudge during poll"
        );
    }

    /// Three nudges arriving before any poll starts collapse to a single
    /// initial poll followed by at most one extra cycle.
    #[tokio::test]
    async fn three_pre_poll_nudges_collapse_to_one_poll() {
        let notify = Arc::new(Notify::new());
        let poll_count = Arc::new(AtomicU32::new(0));

        let n = notify.clone();
        let c = poll_count.clone();

        // Simulate worker: runs polls until no more tokens are pending.
        let worker = tokio::spawn(async move {
            loop {
                // Use timeout to detect when no more tokens are pending.
                let result = tokio::time::timeout(Duration::from_millis(20), n.notified()).await;
                if result.is_err() {
                    break; // No more pending tokens.
                }
                // Simulate a fast poll.
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Fire 3 nudges before the worker has a chance to run.
        notify.notify_one();
        notify.notify_one();
        notify.notify_one();

        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("worker should finish")
            .expect("worker should not panic");

        let count = poll_count.load(Ordering::SeqCst);
        assert!(
            count == 1,
            "3 pre-poll nudges should collapse to exactly 1 poll cycle, got {count}"
        );
    }
}
