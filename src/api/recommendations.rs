//! Reading recommendations endpoints (ADR-059).
//!
//! Thin handlers: everything is computed by
//! `services::recommendation_service` from the local library. Owner-facing
//! only (routed in `owner_routes`): what the user might like next is
//! personal data and is never served to peers.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde_json::{Value, json};

use crate::domain::recommendations::{ScoredRecommendation, TasteProfile};
use crate::infrastructure::AppState;
use crate::services::book_service::ServiceError;
use crate::services::recommendation_service;

#[derive(serde::Deserialize, Default)]
pub struct RecommendationParams {
    pub limit: Option<usize>,
}

/// Hard cap on `?limit=`: keeps a stray large value from serialising the
/// whole library as JSON.
const MAX_LIMIT: usize = 50;

impl RecommendationParams {
    fn clamped_limit(&self) -> Option<usize> {
        self.limit.map(|l| l.min(MAX_LIMIT))
    }
}

fn recommendation_json(rec: &ScoredRecommendation) -> Value {
    json!({
        "book": rec.book,
        "score": rec.score,
        "reasons": rec.reasons.iter().map(|r| json!({
            "type": r.type_key(),
            "value": r.value(),
        })).collect::<Vec<_>>(),
        "source": "library",
    })
}

fn profile_summary_json(profile: &TasteProfile) -> Value {
    json!({
        "top_subjects": profile.top_subjects.iter().map(|(s, _)| s).collect::<Vec<_>>(),
        "favorite_authors": profile.favorite_authors.iter().map(|(a, _)| a).collect::<Vec<_>>(),
        "scored_books_count": profile.scored_books_count,
    })
}

fn error_response(e: ServiceError) -> (StatusCode, Json<Value>) {
    match e {
        ServiceError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Book not found" })),
        ),
        ServiceError::InvalidInput(msg) => (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))),
        // Log the DB detail, never return it: raw SeaORM/SQLite messages leak
        // schema internals (OWASP A05), even on an owner-only route.
        ServiceError::Database(msg) => {
            tracing::error!("recommendation query failed: {msg}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to compute recommendations" })),
            )
        }
    }
}

/// GET /api/books/:id/recommendations - books similar to this one.
pub async fn book_recommendations(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<RecommendationParams>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let recs = recommendation_service::similar_to(state.db(), &id, params.clamped_limit())
        .await
        .map_err(error_response)?;
    Ok(Json(json!({
        "recommendations": recs.iter().map(recommendation_json).collect::<Vec<_>>(),
    })))
}

/// GET /api/recommendations - personal suggestions for the dashboard.
pub async fn personal_recommendations(
    State(state): State<AppState>,
    Query(params): Query<RecommendationParams>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (profile, recs) =
        recommendation_service::suggestions_for_user(state.db(), params.clamped_limit())
            .await
            .map_err(error_response)?;
    Ok(Json(json!({
        "recommendations": recs.iter().map(recommendation_json).collect::<Vec<_>>(),
        "profile_summary": profile_summary_json(&profile),
    })))
}
