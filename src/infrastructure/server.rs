// Server module - Provides reusable HTTP server functionality
// Used by both CLI (main.rs) and FFI (frb.rs)

use axum::Router;
use sea_orm::DatabaseConnection;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use tower_http::cors::{Any, CorsLayer};

use crate::api;
use crate::infrastructure::AppState;

// Global flag to track if server is running
static SERVER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Check if the HTTP server is currently running
pub fn is_server_running() -> bool {
    SERVER_RUNNING.load(Ordering::SeqCst)
}

/// Build the API router with database connection
pub fn build_router(db: DatabaseConnection) -> Router {
    let state = AppState::new(db);
    let api_router = api::api_router_with_state(state.clone());

    // CORS configuration
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new().nest("/api", api_router).layer(cors)
}

/// Find an available port starting from the preferred port on a specific IP
pub fn find_available_port_on_ip(preferred_port: u16, ip: &str) -> Option<u16> {
    // Try preferred port first
    if TcpListener::bind((ip, preferred_port)).is_ok() {
        return Some(preferred_port);
    }

    // Scan next 100 ports
    ((preferred_port + 1)..(preferred_port + 100))
        .find(|&port| TcpListener::bind((ip, port)).is_ok())
}

/// Find an available port starting from the preferred port (0.0.0.0)
pub fn find_available_port(preferred_port: u16) -> Option<u16> {
    find_available_port_on_ip(preferred_port, "0.0.0.0")
}

/// Start the HTTP server on a background task
/// Returns the actual port used
/// Tries 0.0.0.0 first (for P2P), then falls back to 127.0.0.1 (for local-only)
pub async fn start_server(db: DatabaseConnection, preferred_port: u16) -> Result<u16, String> {
    // Check if already running
    if SERVER_RUNNING.load(Ordering::SeqCst) {
        return Err("HTTP server is already running".to_string());
    }

    // Find available port - try 0.0.0.0 first, then fallback to 127.0.0.1
    let (port, addr) = if let Some(p) = find_available_port_on_ip(preferred_port, "0.0.0.0") {
        (p, SocketAddr::from(([0, 0, 0, 0], p)))
    } else if let Some(p) = find_available_port_on_ip(preferred_port, "127.0.0.1") {
        tracing::warn!("⚠️ Falling back to 127.0.0.1 binding (P2P may not work)");
        (p, SocketAddr::from(([127, 0, 0, 1], p)))
    } else {
        return Err("Failed to find available port on 0.0.0.0 or 127.0.0.1".to_string());
    };

    // Build router
    let app = build_router(db);

    // Bind listener
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Failed to bind to {}: {}", addr, e))?;

    // Mark as running
    SERVER_RUNNING.store(true, Ordering::SeqCst);

    tracing::info!("📡 Embedded HTTP server started on {}", addr);

    // Spawn server on background task (won't block FFI)
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("HTTP server error: {}", e);
        }
        SERVER_RUNNING.store(false, Ordering::SeqCst);
    });

    Ok(port)
}

/// Stop the HTTP server (graceful shutdown not implemented yet)
pub fn stop_server() {
    // TODO: Implement graceful shutdown with tokio::sync::watch or similar
    SERVER_RUNNING.store(false, Ordering::SeqCst);
}
