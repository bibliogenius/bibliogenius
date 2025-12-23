use axum::{response::IntoResponse, Json};
use std::process;

pub async fn shutdown() -> impl IntoResponse {
    // Spawn a thread to exit the process after a short delay
    // to allow the response to be sent
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        tracing::info!("🛑 Remote shutdown requested. Exiting...");
        process::exit(0);
    });

    Json(serde_json::json!({ "message": "Server shutting down..." }))
}
