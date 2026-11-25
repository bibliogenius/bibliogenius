//! The one switch that lets this process contact the hub (ADR-073).
//!
//! The consent to register on the hub is asked and stored by the app. The
//! core has its own senders (relay deposits on a book change, the relay
//! poller, the nudge listener, the directory and account clients): they all
//! go through this gate, checked where the request leaves, not sender by
//! sender. Closed, nothing is sent to the hub; what stays on the local
//! network (pairing, direct peer sync, mDNS) never looks at it.
//!
//! Closed by default: the app opens it at startup once it knows the user's
//! choice, and sets it again on every change. A consenting installation is
//! therefore silent for the first moments of a launch: incoming messages
//! wait in the relay mailbox until the first poll, and a change notice that
//! would have left in that interval is not sent (peers catch up at their
//! next sync).
//!
//! The state lives in a `watch` channel rather than a bare flag so a
//! long-lived connection (the nudge WebSocket) can be dropped the moment the
//! gate closes.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

/// Refusal returned by [`ensure_open`]: the request was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubSilenced;

/// Message carried by the error variants built from [`HubSilenced`].
pub const HUB_SILENCED_MESSAGE: &str = "hub contact is not allowed on this installation";

impl std::fmt::Display for HubSilenced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(HUB_SILENCED_MESSAGE)
    }
}

impl std::error::Error for HubSilenced {}

/// Unit tests of this crate predate the gate and talk to mock hubs: they
/// start with the gate open, and none of them may close it (the state is
/// process-wide, tests run in parallel). The gate's own tests live in
/// `tests/`, where each file is its own process. Every other build starts
/// closed.
const OPEN_AT_START: bool = cfg!(test);

fn channel() -> &'static watch::Sender<bool> {
    static GATE: OnceLock<watch::Sender<bool>> = OnceLock::new();
    GATE.get_or_init(|| watch::channel(OPEN_AT_START).0)
}

/// Whether the app gave its answer since the process started. Until then
/// the gate is closed without anyone having said "no".
static TOLD: AtomicBool = AtomicBool::new(false);

/// Open or close the gate. Takes effect on the next request; a listener
/// obtained from [`subscribe`] is woken at once.
pub fn set_open(open: bool) {
    TOLD.store(true, Ordering::Relaxed);
    let changed = channel().send_if_modified(|current| {
        let changed = *current != open;
        *current = open;
        changed
    });
    if changed {
        tracing::info!("Hub gate: {}", if open { "open" } else { "closed" });
    }
}

/// Whether a request to the hub may leave right now.
pub fn is_open() -> bool {
    *channel().borrow()
}

/// Whether the app said this installation does not use the hub. Differs
/// from `!is_open()` during the first moments of a launch, when the gate is
/// closed only because the app has not answered yet: what is decided for
/// good from the hub's absence (the address a peer will cache for a cover)
/// waits for a real "no".
pub fn refused() -> bool {
    TOLD.load(Ordering::Relaxed) && !is_open()
}

tokio::task_local! {
    /// Carried by the task that withdraws the registration, and by nobody
    /// else (see [`with_withdrawal_pass`]).
    static WITHDRAWAL_PASS: ();
}

/// Run `deletion` with a pass through the closed gate (ADR-073 D4).
///
/// Withdrawing the registration silences the device first, then asks the
/// hub to delete what it holds. Those deletions go through the same gated
/// functions as every other request: the gate lets them out because the
/// task that sends them carries this pass. The gate itself stays closed, so
/// every other sender of the core is still refused meanwhile, and a task
/// spawned from `deletion` does not inherit the pass.
///
/// One caller only, the withdrawal (`hub_withdrawal.rs`); a source test
/// fails when another appears.
pub async fn with_withdrawal_pass<F: std::future::Future>(deletion: F) -> F::Output {
    WITHDRAWAL_PASS.scope((), deletion).await
}

fn holds_withdrawal_pass() -> bool {
    WITHDRAWAL_PASS.try_with(|()| ()).is_ok()
}

/// The check every hub request goes through before it is sent.
pub fn ensure_open() -> Result<(), HubSilenced> {
    if is_open() || holds_withdrawal_pass() {
        Ok(())
    } else {
        Err(HubSilenced)
    }
}

/// A receiver that sees every change of the gate, for long-lived connections.
pub fn subscribe() -> watch::Receiver<bool> {
    channel().subscribe()
}

/// Resolve once the gate is closed (at once when it already is).
pub async fn closed(receiver: &mut watch::Receiver<bool>) {
    // The sender is a static: `wait_for` cannot fail with a closed channel.
    let _ = receiver.wait_for(|open| !*open).await;
}

/// Resolve once the gate is open (at once when it already is).
pub async fn opened(receiver: &mut watch::Receiver<bool>) {
    let _ = receiver.wait_for(|open| *open).await;
}
