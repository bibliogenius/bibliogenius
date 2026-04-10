//! Process-wide event bus for peer leaderboard-change notifications (ADR-023).
//!
//! Emitted by the relay poller when a `public_stats_push` message is received
//! from a peer, and consumed by Flutter providers via the FRB stream
//! `subscribe_leaderboard_changes()`.
//!
//! Follows the same design as `catalog_events.rs`:
//!   - Singleton broadcast bus (lock-free emit, zero allocation).
//!   - Slow subscribers lag without blocking emitters.
//!   - Carries no user data, no encrypted payload, no credentials.

use std::sync::OnceLock;
use tokio::sync::broadcast::{self, Receiver, Sender};

/// Maximum buffered events per subscriber. Lagging subscribers skip ahead
/// rather than blocking the emitter (same policy as CatalogEventBus).
const CHANNEL_CAPACITY: usize = 16;

/// Identifies which peer's leaderboard scores changed.
///
/// `peer_id` is the local SQLite row ID for that peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaderboardChangedEvent {
    /// Local peer row ID (from the `peers` table).
    pub peer_id: i32,
}

/// Process-wide leaderboard-change event bus.
pub struct LeaderboardEventBus {
    tx: Sender<LeaderboardChangedEvent>,
}

impl LeaderboardEventBus {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Emit an event. Non-blocking, never panics. Silently dropped when no
    /// subscribers are active (expected steady state when no leaderboard
    /// screen is open).
    pub fn emit(&self, event: LeaderboardChangedEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a fresh receiver. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> Receiver<LeaderboardChangedEvent> {
        self.tx.subscribe()
    }
}

/// Get the process-wide leaderboard event bus. Lazily initialised on first call.
pub fn bus() -> &'static LeaderboardEventBus {
    static INSTANCE: OnceLock<LeaderboardEventBus> = OnceLock::new();
    INSTANCE.get_or_init(LeaderboardEventBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(peer_id: i32) -> LeaderboardChangedEvent {
        LeaderboardChangedEvent { peer_id }
    }

    #[tokio::test]
    async fn emit_with_no_subscriber_does_not_panic() {
        let bus = LeaderboardEventBus::new();
        bus.emit(make(1));
    }

    #[tokio::test]
    async fn subscribe_receives_emitted_event() {
        let bus = LeaderboardEventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(make(42));
        let received = rx.recv().await.expect("event should be received");
        assert_eq!(received.peer_id, 42);
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let bus = LeaderboardEventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.emit(make(7));
        assert_eq!(rx1.recv().await.unwrap().peer_id, 7);
        assert_eq!(rx2.recv().await.unwrap().peer_id, 7);
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_block_emit() {
        let bus = LeaderboardEventBus::new();
        let rx = bus.subscribe();
        drop(rx);
        bus.emit(make(3));
    }

    #[tokio::test]
    async fn slow_subscriber_lags_without_breaking_others() {
        let bus = LeaderboardEventBus::new();
        let mut slow = bus.subscribe();
        let mut fast = bus.subscribe();

        for i in 0..(CHANNEL_CAPACITY + 5) {
            bus.emit(make(i as i32));
            let _ = fast.try_recv();
        }

        let lag_result = slow.try_recv();
        assert!(
            matches!(lag_result, Err(broadcast::error::TryRecvError::Lagged(_))),
            "slow subscriber should report Lagged, got {lag_result:?}"
        );

        bus.emit(make(99));
        let received = fast
            .try_recv()
            .expect("fast should still work after slow lagged");
        assert_eq!(received.peer_id, 99);
    }
}
