//! Process-wide event bus for relay nudge notifications.
//!
//! Conceptually a singleton because nudge events are a process concern,
//! not tied to any particular request or DB connection. Internally backed
//! by a tokio broadcast channel (lock-free, zero allocation on emit).
//!
//! See ADR-017 (Phase 3a) for the wider pipeline.

use std::sync::OnceLock;
use tokio::sync::broadcast::{self, Receiver, Sender};

/// Maximum buffered events per subscriber. Subscribers exceeding this
/// receive `RecvError::Lagged(n)` and skip ahead, they never block emit.
const CHANNEL_CAPACITY: usize = 16;

/// A single relay nudge event. Carries no user data, no encrypted payload,
/// no credentials. The mailbox UUID is process-public information already
/// known to the WS sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NudgeEvent {
    pub mailbox_id: String,
    pub source: NudgeSource,
}

/// Origin of the poll cycle that produced this event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NudgeSource {
    /// Triggered by an instant nudge from the WS sidecar.
    WebSocket,
    /// Triggered by the periodic 20s fallback timer.
    Polling,
    /// Triggered manually (HTTP poll_now endpoint or peer.rs request-response).
    Manual,
}

/// Process-wide nudge event bus. `emit` is lock-free; `subscribe` creates a
/// fresh receiver with its own buffer.
pub struct NudgeBus {
    tx: Sender<NudgeEvent>,
}

impl NudgeBus {
    /// Construct a new bus. Visible to tests; the runtime singleton is
    /// created via [`bus()`].
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Emit an event. Non-blocking, never panics, silently dropped if no
    /// subscribers (the expected steady state when no Flutter UI is alive).
    pub fn emit(&self, event: NudgeEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a fresh receiver. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> Receiver<NudgeEvent> {
        self.tx.subscribe()
    }
}

/// Get the process-wide nudge bus. Lazily initialized on first call.
pub fn bus() -> &'static NudgeBus {
    static INSTANCE: OnceLock<NudgeBus> = OnceLock::new();
    INSTANCE.get_or_init(NudgeBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(id: &str) -> NudgeEvent {
        NudgeEvent {
            mailbox_id: id.to_string(),
            source: NudgeSource::Manual,
        }
    }

    #[tokio::test]
    async fn emit_with_no_subscriber_does_not_panic() {
        let bus = NudgeBus::new();
        bus.emit(make("orphan"));
        // No assertion needed: failure mode would be a panic.
    }

    #[tokio::test]
    async fn subscribe_receives_emitted_event() {
        let bus = NudgeBus::new();
        let mut rx = bus.subscribe();
        bus.emit(make("alpha"));
        let received = rx.recv().await.expect("event should be received");
        assert_eq!(received.mailbox_id, "alpha");
        assert_eq!(received.source, NudgeSource::Manual);
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let bus = NudgeBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.emit(make("beta"));
        assert_eq!(rx1.recv().await.unwrap().mailbox_id, "beta");
        assert_eq!(rx2.recv().await.unwrap().mailbox_id, "beta");
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_block_emit() {
        let bus = NudgeBus::new();
        let rx = bus.subscribe();
        drop(rx);
        // Emit must still succeed even after subscriber dropped.
        bus.emit(make("gamma"));
    }

    #[tokio::test]
    async fn slow_subscriber_lags_without_breaking_others() {
        let bus = NudgeBus::new();
        let mut slow = bus.subscribe();
        let mut fast = bus.subscribe();

        // Saturate slow's buffer beyond CHANNEL_CAPACITY.
        // Drain fast as we go to keep it healthy.
        for i in 0..(CHANNEL_CAPACITY + 5) {
            bus.emit(make(&format!("e{i}")));
            let _ = fast.try_recv();
        }

        // Slow's first recv should report Lagged, not panic, not deadlock.
        let lag_result = slow.try_recv();
        assert!(
            matches!(lag_result, Err(broadcast::error::TryRecvError::Lagged(_))),
            "slow subscriber should report Lagged, got {lag_result:?}"
        );

        // Fast subscriber is still functional after slow's lag.
        bus.emit(make("after_lag"));
        let received = fast
            .try_recv()
            .expect("fast should still work after slow lagged");
        assert_eq!(received.mailbox_id, "after_lag");
    }

    #[tokio::test]
    async fn singleton_bus_returns_same_instance() {
        let b1 = bus();
        let b2 = bus();
        assert!(std::ptr::eq(b1, b2));
    }
}
