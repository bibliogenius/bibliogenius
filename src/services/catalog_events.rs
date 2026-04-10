//! Process-wide event bus for peer catalog-change notifications.
//!
//! Emitted by the relay poller when a `catalog_changed` message is received
//! from a peer, and consumed by Flutter screens via the FRB stream
//! `subscribe_catalog_changes()`.
//!
//! Follows the same design as `nudge_events.rs`:
//!   - Singleton broadcast bus (lock-free emit, zero allocation).
//!   - Slow subscribers lag without blocking emitters.
//!   - Carries no user data, no encrypted payload, no credentials.

use std::sync::OnceLock;
use tokio::sync::broadcast::{self, Receiver, Sender};

/// Maximum buffered events per subscriber. Lagging subscribers skip ahead
/// rather than blocking the emitter (same policy as NudgeBus).
const CHANNEL_CAPACITY: usize = 16;

/// Identifies which peer's catalog changed.
///
/// `peer_library_uuid` is the remote peer's library UUID (from the message
/// payload). `peer_id` is the local SQLite row ID for that peer.
/// Either field can be used for matching on the Flutter side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogChangedEvent {
    /// The peer's library UUID as sent in the `catalog_changed` payload.
    /// Empty string if the payload omitted it (should not happen in practice).
    pub peer_library_uuid: String,
    /// Local peer row ID (from the `peers` table). Zero if lookup failed.
    pub peer_id: i32,
}

/// Process-wide catalog-change event bus.
pub struct CatalogEventBus {
    tx: Sender<CatalogChangedEvent>,
}

impl CatalogEventBus {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Emit an event. Non-blocking, never panics. Silently dropped when no
    /// subscribers are active (expected steady state when no peer library
    /// screen is open).
    pub fn emit(&self, event: CatalogChangedEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a fresh receiver. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> Receiver<CatalogChangedEvent> {
        self.tx.subscribe()
    }
}

/// Get the process-wide catalog event bus. Lazily initialised on first call.
pub fn bus() -> &'static CatalogEventBus {
    static INSTANCE: OnceLock<CatalogEventBus> = OnceLock::new();
    INSTANCE.get_or_init(CatalogEventBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(uuid: &str, peer_id: i32) -> CatalogChangedEvent {
        CatalogChangedEvent {
            peer_library_uuid: uuid.to_string(),
            peer_id,
        }
    }

    #[tokio::test]
    async fn emit_with_no_subscriber_does_not_panic() {
        let bus = CatalogEventBus::new();
        bus.emit(make("uuid-orphan", 1));
    }

    #[tokio::test]
    async fn subscribe_receives_emitted_event() {
        let bus = CatalogEventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(make("uuid-alpha", 42));
        let received = rx.recv().await.expect("event should be received");
        assert_eq!(received.peer_library_uuid, "uuid-alpha");
        assert_eq!(received.peer_id, 42);
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let bus = CatalogEventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.emit(make("uuid-beta", 7));
        assert_eq!(rx1.recv().await.unwrap().peer_library_uuid, "uuid-beta");
        assert_eq!(rx2.recv().await.unwrap().peer_library_uuid, "uuid-beta");
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_block_emit() {
        let bus = CatalogEventBus::new();
        let rx = bus.subscribe();
        drop(rx);
        bus.emit(make("uuid-gamma", 3));
    }

    #[tokio::test]
    async fn slow_subscriber_lags_without_breaking_others() {
        let bus = CatalogEventBus::new();
        let mut slow = bus.subscribe();
        let mut fast = bus.subscribe();

        for i in 0..(CHANNEL_CAPACITY + 5) {
            bus.emit(make(&format!("e{i}"), i as i32));
            let _ = fast.try_recv();
        }

        let lag_result = slow.try_recv();
        assert!(
            matches!(lag_result, Err(broadcast::error::TryRecvError::Lagged(_))),
            "slow subscriber should report Lagged, got {lag_result:?}"
        );

        bus.emit(make("after_lag", 99));
        let received = fast
            .try_recv()
            .expect("fast should still work after slow lagged");
        assert_eq!(received.peer_library_uuid, "after_lag");
    }
}
