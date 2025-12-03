use axum::Json;
use serde_json::{json, Value};

#[utoipa::path(
    get,
    path = "/api/health",
    responses(
        (status = 200, description = "Service is healthy")
    )
)]
pub async fn health_check() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "bibliogenius",
        "version": env!("CARGO_PKG_VERSION")
    }))
}
