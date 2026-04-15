//! Process-wide event bus for peer profile-change notifications (ADR-025).
//!
//! Emitted by the relay poller when a `profile_changed` message is received
//! from a peer, and consumed by Flutter via the FRB stream
//! `subscribe_profile_changes()`.
//!
//! A `profile_changed` nudge is opaque at transport level; the `changed`
//! field lists which profile properties were updated (`"avatar"`,
//! `"library_name"`, etc.) so future fields extend the payload without a
//! protocol bump. Flutter drives the actual pull via
//! `try_peer_avatar_pull(peer_id)`.

use std::sync::OnceLock;
use tokio::sync::broadcast::{self, Receiver, Sender};

/// Maximum buffered events per subscriber. Lagging subscribers skip ahead
/// rather than blocking the emitter (same policy as NudgeBus / CatalogBus).
const CHANNEL_CAPACITY: usize = 16;

/// Identifies which peer's profile changed and which fields are stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileChangedEvent {
    /// Local peer row ID (from the `peers` table). Zero if lookup failed.
    pub peer_id: i32,
    /// Which profile fields changed (`"avatar"`, `"library_name"`, ...).
    /// The list is advisory — receivers typically re-pull all fields.
    pub changed: Vec<String>,
}

pub struct ProfileEventBus {
    tx: Sender<ProfileChangedEvent>,
}

impl ProfileEventBus {
    pub(crate) fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Emit an event. Non-blocking, never panics. Silently dropped when no
    /// subscribers are active.
    pub fn emit(&self, event: ProfileChangedEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a fresh receiver. Drop the receiver to unsubscribe.
    pub fn subscribe(&self) -> Receiver<ProfileChangedEvent> {
        self.tx.subscribe()
    }
}

/// Get the process-wide profile event bus. Lazily initialised on first call.
pub fn bus() -> &'static ProfileEventBus {
    static INSTANCE: OnceLock<ProfileEventBus> = OnceLock::new();
    INSTANCE.get_or_init(ProfileEventBus::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(peer_id: i32, changed: &[&str]) -> ProfileChangedEvent {
        ProfileChangedEvent {
            peer_id,
            changed: changed.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn emit_with_no_subscriber_does_not_panic() {
        let bus = ProfileEventBus::new();
        bus.emit(make(1, &["avatar"]));
    }

    #[tokio::test]
    async fn subscribe_receives_emitted_event() {
        let bus = ProfileEventBus::new();
        let mut rx = bus.subscribe();
        bus.emit(make(42, &["avatar"]));
        let received = rx.recv().await.expect("event should be received");
        assert_eq!(received.peer_id, 42);
        assert_eq!(received.changed, vec!["avatar".to_string()]);
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let bus = ProfileEventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.emit(make(7, &["avatar", "library_name"]));
        assert_eq!(rx1.recv().await.unwrap().peer_id, 7);
        assert_eq!(rx2.recv().await.unwrap().peer_id, 7);
    }
}
