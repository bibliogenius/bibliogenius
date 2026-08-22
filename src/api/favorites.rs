//! Favorites endpoints (ADR-064).
//!
//! Thin HTTP mirror of the FFI favorites surface (Rule F3 "both, always"):
//! everything delegates to `services::favorites_service`. Owner-facing only
//! (routed in `owner_routes`): what the user loves is personal data and is
//! never served to peers.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use crate::domain::DomainError;
use crate::infrastructure::AppState;
use crate::services::favorites_service;

fn error_response(e: DomainError) -> (StatusCode, Json<Value>) {
    match e {
        DomainError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Resource not found" })),
        ),
        DomainError::Validation(msg) => (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))),
        // Log the detail, never return it: raw SeaORM/SQLite messages leak
        // schema internals (OWASP A05), even on an owner-only route.
        other => {
            tracing::error!("favorites operation failed: {other:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Favorites operation failed" })),
            )
        }
    }
}

/// GET /api/favorites - all favorite book ids, one pass.
pub async fn list_favorites(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ids = favorites_service::get_favorite_book_ids(state.db())
        .await
        .map_err(error_response)?;
    Ok(Json(json!({ "book_ids": ids })))
}

/// POST /api/favorites/:book_id/toggle - flip a book's favorite state.
pub async fn toggle_favorite(
    State(state): State<AppState>,
    Path(book_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let is_favorite = favorites_service::toggle_favorite_book(state.db(), &book_id)
        .await
        .map_err(error_response)?;
    Ok(Json(json!({ "is_favorite": is_favorite })))
}

/// POST /api/favorites/seed - Reader-preset seeding, gate enforced in the
/// service. Returns whether the collection was created.
pub async fn seed_favorites(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let created = favorites_service::seed_favorites_collection(state.db())
        .await
        .map_err(error_response)?;
    Ok(Json(json!({ "created": created })))
}

/// GET /api/favorites/adoption-candidate - the manual collection to propose
/// for one-shot adoption, if any.
pub async fn adoption_candidate(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let candidate = favorites_service::get_favorites_adoption_candidate(state.db())
        .await
        .map_err(error_response)?;
    Ok(Json(json!({ "candidate": candidate })))
}

/// POST /api/favorites/adopt/:collection_id - adopt a manual collection as
/// THE favorites collection (source flip, members kept).
pub async fn adopt(
    State(state): State<AppState>,
    Path(collection_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    favorites_service::adopt_favorites_collection(state.db(), &collection_id)
        .await
        .map_err(error_response)?;
    Ok(Json(json!({ "adopted": true })))
}
