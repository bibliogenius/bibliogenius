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

/// Start the HTTP server on the specified port (FFI)
/// This is required for P2P functionality in standalone mode
/// If the specified port is occupied, tries the next 10 ports automatically
pub async fn start_server(port: u16) -> Result<u16, String> {
    let db = db().ok_or("Database not initialized")?.clone();

    // Try the specified port and fall back to alternatives if occupied
    let max_attempts = 10;
    let mut last_error = String::new();

    for offset in 0..max_attempts {
        let try_port = port.saturating_add(offset);
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

                let shared_id_svc = IDENTITY_SERVICE
                    .get_or_init(|| crate::services::IdentityService::new(db.clone()));
                let state = crate::infrastructure::AppState::with_identity_service(
                    db,
                    std::sync::Arc::new(shared_id_svc.clone()),
                );
                state.set_server_port(actual_port);
                // Store globally so FFI handlers (create_book, delete_book) can
                // trigger catalog-change notifications without going through HTTP.
                let _ = GLOBAL_APP_STATE.set(state.clone());

                // Spawn relay poller (checks relay hub for incoming messages)
                let poller_state = state.clone();
                tokio::spawn(async move {
                    crate::services::relay_poller::start_relay_polling(
                        poller_state,
                        std::time::Duration::from_secs(20),
                    )
                    .await;
                });

                // Spawn WS nudge listener (instant relay notifications, ADR-017)
                let ws_state = state.clone();
                tokio::spawn(async move {
                    crate::services::ws_nudge::start_ws_nudge(ws_state).await;
                });

                // Spawn operation processor (applies pending ops from device sync)
                let processor_db = state.db().clone();
                tokio::spawn(async move {
                    crate::sync::processor::run_processor(processor_db).await;
                });

                // Spawn delta sync retention pruner (ADR-028 D5)
                crate::services::oplog_pruner::spawn(state.db().clone());

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
                    tracing::error!(
                        "💀 FFI Server task ended on port {} - server is no longer running!",
                        server_port
                    );
                });

                if offset > 0 {
                    tracing::info!(
                        "✅ FFI: Port {} was occupied, server started on port {}",
                        port,
                        actual_port
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
        port,
        port + max_attempts - 1,
        last_error
    ))
}
