// Event subscriptions and peer deltas: catalog, profile, leaderboard, relay nudges.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Relay Nudge Stream (FFI, ADR-017 Phase 3a) ============
//
// Lets Flutter subscribe to a stream of relay nudge events emitted by
// `relay_poller::poll_once()` whenever fresh relay data has been written to
// the local DB. Flutter listeners can use this to refresh providers
// immediately, instead of waiting for their own 30s polling timers.
//
// The existing polling timers in Flutter remain in place as a safety net
// during this rollout (Phase 3a "additive" approach). They will be removed
// in Phase 3b once the stream has been validated in production.

/// FFI-safe view of a relay nudge event.
///
/// `source` is one of: "websocket" (instant nudge), "polling" (fallback timer),
/// "manual" (user-triggered or peer.rs request-response).
#[frb(dart_metadata=("freezed"))]
pub struct FrbNudgeEvent {
    pub mailbox_id: String,
    pub source: String,
}

/// FFI-safe view of a peer catalog-change event.
///
/// Emitted when a peer sends a `catalog_changed` relay message, indicating
/// that they added or deleted a book. Flutter screens showing that peer's
/// library should trigger a re-sync on receipt.
///
/// Match by `peer_id` (local SQLite row ID) or `peer_library_uuid` (remote
/// UUID from the message payload). Both are provided so callers can use
/// whichever is available in their context.
#[frb(dart_metadata=("freezed"))]
pub struct FrbCatalogChangedEvent {
    /// Remote peer's library UUID (from the message payload).
    pub peer_library_uuid: String,
    /// Local peer row ID from the `peers` table. Zero if the lookup failed.
    pub peer_id: i32,
    /// True when the Rust side already applied a delta window to
    /// `peer_books` before emitting this event (ADR-029). Flutter should
    /// skip the legacy `relay_library_request("manifest")` full-catalog
    /// pull and simply re-read the local cache. False when the delta path
    /// was not taken and the legacy flow must run.
    pub delta_applied: bool,
}

fn nudge_source_label(source: crate::services::nudge_events::NudgeSource) -> String {
    use crate::services::nudge_events::NudgeSource;
    match source {
        NudgeSource::WebSocket => "websocket".to_string(),
        NudgeSource::Polling => "polling".to_string(),
        NudgeSource::Manual => "manual".to_string(),
    }
}

/// Subscribe to the relay nudge event stream.
///
/// Each emitted event indicates that `poll_once()` finished processing at
/// least one message and persisted it to the local DB. Flutter consumers
/// should refresh the relevant providers (notifications, loan requests,
/// peer libraries) on receipt.
///
/// The function returns immediately after spawning a forwarding task. The
/// task lives until the Dart side drops the StreamSink.
/// Subscribe to the catalog-change event stream.
///
/// Each emitted event indicates that a peer added or deleted a book and
/// their catalog is now different from what the local device has cached.
/// Flutter consumers (typically `PeerBookListScreen`) should trigger a
/// re-sync when they receive an event matching the displayed peer.
///
/// The stream lives until the Dart side drops the `StreamSink`. Multiple
/// concurrent subscribers each receive their own independent copy of every
/// event (broadcast semantics). A slow subscriber lags without blocking
/// the emitter.
/// Attempt a delta sync against a peer via E2EE (ADR-029).
///
/// Returns `true` when a delta window was successfully fetched and applied
/// to `peer_books` - the caller should SKIP the legacy
/// `relay_library_request("manifest")` loop and simply re-read the local
/// cache. Returns `false` on any non-applied outcome
/// (`ResetRequired`, `FallbackRequired`, `E2eeUnavailable`, transport
/// error) - the caller should run the legacy full-catalog flow as before.
///
/// Designed to be called from the Flutter `subscribe_catalog_changes`
/// handler before triggering a full sync, so the delta path replaces the
/// full pull whenever it succeeds.
pub async fn try_peer_catalog_delta(peer_id: i32) -> bool {
    try_peer_catalog_delta_detailed(peer_id)
        .await
        .starts_with("applied:")
}

/// Same as [`try_peer_catalog_delta`] but returns a descriptive string so
/// Flutter can surface the exact outcome without depending on the FFI log
/// file (invisible on iOS). Format:
/// - `applied:<ops>:<cursor>:<has_more>`: delta applied.
/// - `fallback_required`: peer did not respond.
/// - `e2ee_unavailable`: no E2EE capability.
/// - `reset_required`: cursor pruned upstream, responder did not populate
///   `current_cursor` (older codebase).
/// - `reset_required:<N>`: cursor pruned upstream, responder reports its
///   current `operation_log` max id as `N`. The caller SHOULD persist `N`
///   via [`set_peer_delta_cursor`] only after a successful legacy full
///   sync, to break the reset loop.
/// - `no_state`: AppState not initialised.
/// - `error:<message>`: transport or DB error.
pub async fn try_peer_catalog_delta_detailed(peer_id: i32) -> String {
    use crate::services::peer_delta_sync::{self, DeltaSyncOutcome};

    let Some(state) = global_app_state() else {
        return "no_state".to_string();
    };

    match peer_delta_sync::fetch_and_apply_peer_delta(state, peer_id).await {
        Ok(DeltaSyncOutcome::Applied {
            operations_applied,
            latest_cursor,
            has_more,
        }) => format!("applied:{operations_applied}:{latest_cursor}:{has_more}"),
        Ok(DeltaSyncOutcome::FallbackRequired) => "fallback_required".to_string(),
        Ok(DeltaSyncOutcome::E2eeUnavailable) => "e2ee_unavailable".to_string(),
        Ok(DeltaSyncOutcome::ResetRequired { current_cursor }) => match current_cursor {
            Some(n) => format!("reset_required:{n}"),
            None => "reset_required".to_string(),
        },
        Err(e) => format!("error:{e}"),
    }
}

/// Persist `peers.last_delta_cursor` for the given peer.
///
/// Flutter calls this after a successful legacy full-catalog sync that was
/// triggered by a `reset_required:<N>` outcome, passing the responder's
/// reported `current_cursor`. This breaks the reset loop by letting the
/// next sync resume as a delta.
///
/// Returns `Ok(())` on success, or a descriptive error string on DB
/// failure / unknown peer id. Safe to call with a cursor of 0 (no-op for
/// peers that have never had any operations).
pub async fn set_peer_delta_cursor(peer_id: i32, cursor: i64) -> Result<(), String> {
    let Some(state) = global_app_state() else {
        return Err("no_state".to_string());
    };
    crate::services::peer_delta_sync::set_peer_last_delta_cursor(state.db(), peer_id, cursor)
        .await
        .map_err(|e| format!("set_peer_last_delta_cursor: {e}"))
}

/// Persist a refreshed `peers.library_uuid` for the given peer (ADR-030).
///
/// The E2EE-signed manifest from a peer carries its current `library_uuid`.
/// When that value diverges from the locally persisted one, the local row
/// is stale (historical drift from an older pairing code path). This helper
/// adopts the manifest value so all downstream lookups (hub directory
/// fallback, event UUID matching on later mounts) see the current identity.
///
/// Trust model: only call this with a `new_uuid` read from an ENVELOPE
/// that successfully verified against `peers.public_key` (ed25519). The
/// signature check on that path is what binds the uuid to the peer identity.
/// Skipping it would let any relay forwarder inject an arbitrary uuid.
/// `peer_book` rows are intentionally left untouched: they key on
/// `peer_id`, not `library_uuid`, and the enclosing manifest sync pass is
/// already about to refresh them via upsert (a premature purge would flash
/// an empty library in the UI before the pages arrive).
///
/// Idempotent: writing the same uuid twice is a no-op; writing a null or
/// empty string is rejected to avoid clearing a healthy value by accident.
///
/// Returns `Ok(true)` when the stored uuid changed (Flutter may log it),
/// `Ok(false)` when the value was already current.
pub async fn update_peer_library_uuid(peer_id: i32, new_uuid: String) -> Result<bool, String> {
    if new_uuid.trim().is_empty() {
        return Err("update_peer_library_uuid: refusing empty uuid".to_string());
    }
    let Some(state) = global_app_state() else {
        return Err("no_state".to_string());
    };
    crate::services::peer_identity_sync::persist_peer_library_uuid(state.db(), peer_id, &new_uuid)
        .await
        .map_err(|e| format!("persist_peer_library_uuid: {e}"))
}

pub async fn subscribe_catalog_changes(
    sink: crate::frb_generated::StreamSink<FrbCatalogChangedEvent>,
) -> Result<(), String> {
    let mut rx = crate::services::catalog_events::bus().subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let frb_event = FrbCatalogChangedEvent {
                        peer_library_uuid: event.peer_library_uuid,
                        peer_id: event.peer_id,
                        delta_applied: event.delta_applied,
                    };
                    if sink.add(frb_event).is_err() {
                        tracing::debug!(
                            "Catalog change stream: Dart sink closed, ending forwarder"
                        );
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Catalog change stream: subscriber lagged, dropped {n} events");
                    // Recoverable: next recv() returns the oldest buffered event.
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::error!("Catalog change stream: bus sender closed unexpectedly");
                    break;
                }
            }
        }
    });

    Ok(())
}

// ============ Profile Change Stream (ADR-025) ============

/// FFI-safe view of a peer profile-change event.
///
/// Emitted when a peer sends a `profile_changed` relay message after they
/// edit their avatar (or, in the future, another profile field). Flutter
/// should call `try_peer_avatar_pull(peer_id)` on receipt to fetch the
/// fresh values over E2EE and update the local `peers` row.
#[frb(dart_metadata=("freezed"))]
pub struct FrbProfileChangedEvent {
    /// Local peer row ID from the `peers` table.
    pub peer_id: i32,
    /// Which profile fields the sender marked as changed
    /// (`"avatar"`, `"library_name"`, ...). Advisory: the receiver normally
    /// re-pulls all fields in one round-trip.
    pub changed: Vec<String>,
}

/// Subscribe to the profile-change event stream (ADR-025).
///
/// Each emitted event indicates that a peer's profile (today: avatar)
/// changed. Flutter should pull the new values via
/// `try_peer_avatar_pull(peer_id)`. The subscription is intended to be
/// registered once at app level (`AvatarSyncService`) so avatars stay
/// fresh across every screen without per-screen wiring.
///
/// The stream lives until the Dart side drops the `StreamSink`.
pub async fn subscribe_profile_changes(
    sink: crate::frb_generated::StreamSink<FrbProfileChangedEvent>,
) -> Result<(), String> {
    let mut rx = crate::services::profile_events::bus().subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let frb_event = FrbProfileChangedEvent {
                        peer_id: event.peer_id,
                        changed: event.changed,
                    };
                    if sink.add(frb_event).is_err() {
                        tracing::debug!(
                            "Profile change stream: Dart sink closed, ending forwarder"
                        );
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Profile change stream: subscriber lagged, dropped {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::error!("Profile change stream: bus sender closed unexpectedly");
                    break;
                }
            }
        }
    });

    Ok(())
}

/// Pull a peer's avatar (and `library_name`) over E2EE (ADR-025).
///
/// Returns `true` when at least one field changed and was persisted to the
/// local `peers` row. Returns `false` when the peer is up to date, the
/// peer did not respond, or E2EE is unavailable. Errors are converted to
/// `false` and logged (the caller's UI should degrade gracefully to the
/// cached avatar).
///
/// Designed to be called from the Flutter `subscribe_profile_changes`
/// handler (`AvatarSyncService`) whenever a peer emits a `profile_changed`
/// nudge. Also safe to call opportunistically on first-seen of a relay-only
/// peer.
pub async fn try_peer_avatar_pull(peer_id: i32) -> bool {
    let Some(state) = global_app_state() else {
        tracing::warn!("try_peer_avatar_pull: AppState not initialized");
        return false;
    };

    match crate::api::peer::try_pull_avatar_via_relay(state, peer_id).await {
        Ok(changed) => changed,
        Err(e) => {
            tracing::warn!("try_peer_avatar_pull: peer {peer_id} error: {e}");
            false
        }
    }
}

pub async fn subscribe_relay_nudges(
    sink: crate::frb_generated::StreamSink<FrbNudgeEvent>,
) -> Result<(), String> {
    let mut rx = crate::services::nudge_events::bus().subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let frb_event = FrbNudgeEvent {
                        mailbox_id: event.mailbox_id,
                        source: nudge_source_label(event.source),
                    };
                    if sink.add(frb_event).is_err() {
                        // Dart dropped the subscription, exit cleanly.
                        tracing::debug!("Relay nudge stream: Dart sink closed, ending forwarder");
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Relay nudge stream: subscriber lagged, dropped {n} events");
                    // Lagged is recoverable; the next recv() returns the
                    // oldest still-buffered event.
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Should never happen: the bus's Sender is held in a static
                    // OnceLock that lives for the entire process lifetime.
                    tracing::error!("Relay nudge stream: bus sender closed unexpectedly");
                    break;
                }
            }
        }
    });

    Ok(())
}

// ============ Leaderboard Change Stream (ADR-023) ============

/// FFI-safe view of a leaderboard-change event.
///
/// Emitted when a peer sends a `public_stats_push` relay message, indicating
/// that they beat their personal best in a game or gained a gamification level.
/// Flutter providers showing network leaderboards should trigger a re-load
/// on receipt.
#[frb(dart_metadata=("freezed"))]
pub struct FrbLeaderboardChangedEvent {
    /// Local peer row ID from the `peers` table.
    pub peer_id: i32,
}

/// Stream of leaderboard-change events from peers (ADR-023).
///
/// Each emitted event indicates that a peer pushed updated scores via
/// `public_stats_push` and the local cache has been updated. Flutter
/// consumers (game leaderboard screens) should reload network scores.
///
/// The stream lives until the Dart side drops the `StreamSink`. Multiple
/// concurrent subscribers each receive their own independent copy of every
/// event (broadcast semantics). A slow subscriber lags without blocking
/// the emitter.
pub async fn subscribe_leaderboard_changes(
    sink: crate::frb_generated::StreamSink<FrbLeaderboardChangedEvent>,
) -> Result<(), String> {
    let mut rx = crate::services::leaderboard_events::bus().subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let frb_event = FrbLeaderboardChangedEvent {
                        peer_id: event.peer_id,
                    };
                    if sink.add(frb_event).is_err() {
                        tracing::debug!(
                            "Leaderboard change stream: Dart sink closed, ending forwarder"
                        );
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "Leaderboard change stream: subscriber lagged, dropped {n} events"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::error!("Leaderboard change stream: bus sender closed unexpectedly");
                    break;
                }
            }
        }
    });

    Ok(())
}
