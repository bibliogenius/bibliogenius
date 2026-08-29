// Full app reset and embedded HTTP server startup.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Reset API ============

/// Reset the entire application - deletes all data from all tables
/// This is irreversible and should be used with caution
pub async fn reset_app() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;

    // Unregister from hub directory BEFORE deleting local data (needs write_token).
    // Fire-and-forget: failure should not block local reset.
    {
        let hub_svc = crate::services::hub_directory_service::HubDirectoryService::new();
        match hub_svc.delete_profile(db).await {
            Ok(()) => tracing::info!("Hub directory profile deleted during reset"),
            Err(e) => tracing::warn!("Hub directory deregistration failed (non-fatal): {e}"),
        }
    }

    use crate::models::{
        author, book, book_authors, book_tags, collection, collection_book, contact, copy,
        installation_profile, library, library_config, loan, notification, operation_log,
        p2p_outgoing_request, p2p_request, peer, peer_book, tag, user,
    };
    use sea_orm::{ConnectionTrait, EntityTrait};

    // Helper macro to delete all from a table
    macro_rules! delete_all {
        ($entity:ident) => {
            if let Err(e) = $entity::Entity::delete_many().exec(db).await {
                return Err(format!("Failed to delete {}: {}", stringify!($entity), e));
            }
        };
    }

    // Delete in order of dependencies (child tables first)
    delete_all!(loan);
    delete_all!(copy);
    delete_all!(collection_book);
    delete_all!(collection);
    delete_all!(book_authors);
    delete_all!(book_tags);
    delete_all!(book);
    delete_all!(author);
    delete_all!(tag);

    delete_all!(p2p_outgoing_request);
    delete_all!(p2p_request);
    delete_all!(peer_book);
    delete_all!(peer);
    delete_all!(contact);

    delete_all!(notification);
    delete_all!(operation_log);

    delete_all!(library_config);
    delete_all!(library);
    delete_all!(installation_profile);

    // Delete users too for complete reset
    delete_all!(user);

    // Clean hub directory config (raw SQL - no SeaORM entity)
    if let Err(e) = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM hub_directory_config".to_owned(),
        ))
        .await
    {
        tracing::warn!("Failed to delete hub_directory_config: {}", e);
        // Non-fatal: table may not exist on older installs
    }

    Ok("App reset successfully - all data cleared".to_string())
}

// ============ HTTP Server (FFI) ============

/// Port the embedded HTTP server is bound to in this process, `0` when it has
/// never started.
static SERVER_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// Whether a listener spawned here is still serving. Cleared by the serving
/// task when `axum::serve` returns.
static SERVER_LISTENING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the long-lived background workers (relay poller, WS nudge listener,
/// operation processor, oplog pruner) have been spawned. They outlive the
/// listener, so a restart must not spawn a second copy of each.
static BACKGROUND_WORKERS_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Serializes start attempts: without it two concurrent callers could both pass
/// the "already listening" check and bind two listeners on two ports.
static SERVER_START_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn server_start_lock() -> &'static tokio::sync::Mutex<()> {
    SERVER_START_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// What a start attempt should do, given what this process already runs.
#[derive(Debug, PartialEq, Eq)]
enum ServerStartDecision {
    /// A listener is already serving: hand this port back, bind nothing.
    Reuse(u16),
    /// Nothing is serving: bind, starting the search at this port.
    Bind(u16),
}

/// Decide between reusing the live listener and binding a new one.
///
/// Kept free of globals so the policy is testable: the two traps it closes are
/// a second bind while the first listener is alive, and a restart drifting off
/// the port peers hold a URL for.
fn decide_server_start(
    listening: bool,
    known_port: u16,
    requested_port: u16,
) -> ServerStartDecision {
    if listening && known_port != 0 {
        return ServerStartDecision::Reuse(known_port);
    }
    // A listener that died left its port free: come back on it rather than
    // drifting, peers store URLs built on it.
    ServerStartDecision::Bind(if known_port != 0 {
        known_port
    } else {
        requested_port
    })
}

/// Spawn the workers that must run for the whole life of the process: relay
/// polling, the WS nudge listener (ADR-017), the device-sync operation
/// processor and the delta-sync retention pruner (ADR-028 D5).
///
/// Separated from `start_server` because their lifetime is the process, not the
/// listener: they are spawned on the first successful start only.
fn spawn_background_workers(state: &crate::infrastructure::AppState) {
    // Relay poller (checks relay hub for incoming messages)
    let poller_state = state.clone();
    tokio::spawn(async move {
        crate::services::relay_poller::start_relay_polling(
            poller_state,
            std::time::Duration::from_secs(20),
        )
        .await;
    });

    // WS nudge listener (instant relay notifications, ADR-017)
    let ws_state = state.clone();
    tokio::spawn(async move {
        crate::services::ws_nudge::start_ws_nudge(ws_state).await;
    });

    // Operation processor (applies pending ops from device sync)
    let processor_db = state.db().clone();
    tokio::spawn(async move {
        crate::sync::processor::run_processor(processor_db).await;
    });

    // Delta sync retention pruner (ADR-028 D5)
    crate::services::oplog_pruner::spawn(state.db().clone());
}

/// Start the HTTP server on the specified port (FFI).
/// This is required for P2P functionality in standalone mode.
///
/// Idempotent per process: called while the server is already listening, it
/// returns the live port instead of binding a second one. Android destroys the
/// activity without killing the process, so reopening the app replays this call
/// against a listener that is still alive. The previous behaviour slid to the
/// next free port, which made the app report a port conflict against itself,
/// left peers holding an unreachable URL on the preferred port, and spawned a
/// second copy of every background worker. A health-check driven restart
/// (`ApiService.ensureServerRunning` on resume) fell into the same trap.
///
/// Only a genuinely foreign occupant moves the port, and it is searched from
/// the port this process last held. Tries up to 10 ports.
pub async fn start_server(port: u16) -> Result<u16, String> {
    use std::sync::atomic::Ordering;

    // Held for the whole attempt: two callers must not bind two listeners.
    let _start_guard = server_start_lock().lock().await;

    let preferred_port = match decide_server_start(
        SERVER_LISTENING.load(Ordering::SeqCst),
        SERVER_PORT.load(Ordering::SeqCst),
        port,
    ) {
        ServerStartDecision::Reuse(running_port) => {
            tracing::info!(
                "FFI: HTTP server already listening on port {}, reusing it",
                running_port
            );
            return Ok(running_port);
        }
        ServerStartDecision::Bind(preferred_port) => preferred_port,
    };

    let db = db().ok_or("Database not initialized")?.clone();

    // Try the preferred port and fall back to alternatives if occupied
    let max_attempts = 10;
    let mut last_error = String::new();

    for offset in 0..max_attempts {
        let try_port = preferred_port.saturating_add(offset);
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], try_port));

        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map_err(|e| format!("Failed to get local address: {}", e))?
                    .port();

                // Create a shared IdentityService and register it in the global
                // OnceLock so that init_identity_ffi() (called later by Flutter)
                // initializes the SAME instance. IdentityService uses Arc<OnceCell>
                // internally, so clones share the same identity state.
                // Safety: if no user exists (stale DB after macOS reinstall),
                // turn off hub directory listing to protect user privacy.
                // Application Support persists across macOS uninstall/reinstall.
                {
                    use sea_orm::{ConnectionTrait, Statement};
                    let be = db.get_database_backend();
                    let no_user = db
                        .query_one(Statement::from_string(
                            be,
                            "SELECT COUNT(*) AS cnt FROM users".to_owned(),
                        ))
                        .await
                        .ok()
                        .flatten()
                        .and_then(|r| r.try_get::<i32>("", "cnt").ok())
                        .unwrap_or(0)
                        == 0;
                    if no_user {
                        let _ = db
                            .execute(Statement::from_string(
                                be,
                                "UPDATE hub_directory_config SET is_listed = 0 WHERE is_listed = 1"
                                    .to_owned(),
                            ))
                            .await;
                    }
                }

                // Reuse the AppState registered by an earlier start. FFI handlers
                // (create_book, delete_book) read it through GLOBAL_APP_STATE, a
                // OnceLock that only ever accepts its first value: building a fresh
                // state on a restart would leave those handlers on the retired one,
                // still advertising the port this server no longer listens on.
                let state = match global_app_state() {
                    Some(existing) => existing.clone(),
                    None => {
                        let shared_id_svc = IDENTITY_SERVICE
                            .get_or_init(|| crate::services::IdentityService::new(db.clone()));
                        let fresh = crate::infrastructure::AppState::with_identity_service(
                            db.clone(),
                            std::sync::Arc::new(shared_id_svc.clone()),
                        );
                        // Store globally so FFI handlers (create_book, delete_book) can
                        // trigger catalog-change notifications without going through HTTP.
                        let _ = GLOBAL_APP_STATE.set(fresh.clone());
                        fresh
                    }
                };
                state.set_server_port(actual_port);

                // Long-lived workers, spawned once per process. They hold their own
                // clone of the connection and survive a listener restart, so
                // re-spawning would duplicate every relay poll and pruning pass.
                if BACKGROUND_WORKERS_STARTED
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    spawn_background_workers(&state);
                }

                let api = crate::api::api_router_with_state(state);
                // Allow CORS for all origins/methods/headers for P2P ease
                let cors = CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any);

                let app = axum::Router::new()
                    .route(
                        "/invite",
                        axum::routing::get(crate::api::invite_page::invite_page),
                    )
                    .nest("/api", api)
                    .layer(cors);

                // Published before the serving task starts so a concurrent caller
                // released by the start lock reuses this port instead of binding
                // the next one.
                SERVER_PORT.store(actual_port, Ordering::SeqCst);
                SERVER_LISTENING.store(true, Ordering::SeqCst);

                // Spawn server in background with panic catching
                let server_port = actual_port;
                tokio::spawn(async move {
                    tracing::info!("🚀 FFI Server task starting on port {}", server_port);
                    // connect_info exposes the caller's SocketAddr in request
                    // extensions, which the LoopbackOnly guard on device
                    // management endpoints relies on (also aligns this FFI
                    // server with the standalone and desktop entry points).
                    match axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .await
                    {
                        Ok(()) => {
                            tracing::warn!(
                                "⚠️ FFI Server task exited normally on port {} (this is unexpected)",
                                server_port
                            );
                        }
                        Err(e) => {
                            tracing::error!("❌ FFI Server Error on port {}: {}", server_port, e);
                        }
                    }
                    // Lets a later start_server bind again instead of handing back
                    // a port nothing listens on. SERVER_PORT is deliberately kept:
                    // the restart aims at the same port.
                    SERVER_LISTENING.store(false, Ordering::SeqCst);
                    tracing::error!(
                        "💀 FFI Server task ended on port {} - server is no longer running!",
                        server_port
                    );
                });

                if offset > 0 {
                    tracing::warn!(
                        "⚠️ FFI: Port {} is held by another process, server started on port {} instead: peers holding our {} URL cannot reach us directly",
                        preferred_port,
                        actual_port,
                        preferred_port
                    );
                } else {
                    tracing::info!("✅ FFI: Server started on port {}", actual_port);
                }
                return Ok(actual_port);
            }
            Err(e) => {
                last_error = format!("{}", e);
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    tracing::debug!("Port {} occupied, trying {}", try_port, try_port + 1);
                    continue;
                } else {
                    // Non-recoverable error
                    return Err(format!("Failed to bind to port {}: {}", try_port, e));
                }
            }
        }
    }

    Err(format!(
        "Failed to bind to any port from {} to {}: {}",
        preferred_port,
        preferred_port.saturating_add(max_attempts - 1),
        last_error
    ))
}

#[cfg(test)]
mod server_start_decision_tests {
    use super::{ServerStartDecision, decide_server_start};

    #[test]
    fn reuses_the_live_listener_instead_of_binding_a_second_one() {
        // Android destroys the activity without killing the process: the relaunch
        // replays start_server against a listener that is still serving.
        assert_eq!(
            decide_server_start(true, 8000, 8000),
            ServerStartDecision::Reuse(8000)
        );
    }

    #[test]
    fn reuses_the_live_listener_whatever_port_the_caller_asks_for() {
        // A health-check driven restart passes the port it believes in: the live
        // one wins, so a flaky probe cannot move the server.
        assert_eq!(
            decide_server_start(true, 8003, 8000),
            ServerStartDecision::Reuse(8003)
        );
    }

    #[test]
    fn binds_the_requested_port_on_a_first_start() {
        assert_eq!(
            decide_server_start(false, 0, 8000),
            ServerStartDecision::Bind(8000)
        );
    }

    #[test]
    fn a_restart_returns_to_the_port_it_held_rather_than_drifting() {
        // Peers store URLs built on the port we advertised: a restart that
        // silently moved would leave every one of them unable to reach us.
        assert_eq!(
            decide_server_start(false, 8003, 8000),
            ServerStartDecision::Bind(8003)
        );
    }

    #[test]
    fn a_zero_known_port_is_never_treated_as_a_live_listener() {
        assert_eq!(
            decide_server_start(true, 0, 8000),
            ServerStartDecision::Bind(8000)
        );
    }
}
