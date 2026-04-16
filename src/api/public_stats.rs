//! Public stats bundle endpoint (ADR-022 Phase 1 optimization).
//!
//! Returns the same `PublicStatsBundle` payload that the E2EE relay produces
//! in response to a `public_stats_request`. Lets LAN peers fetch all four
//! mini-game scores + gamification in a single HTTP round-trip instead of
//! 4 sequential calls (`/api/config` + 3 `public-best` endpoints).
//!
//! Public endpoint (no auth): callable by any peer that can reach the server.

use axum::{Json, extract::State};
use serde_json::Value;

use crate::infrastructure::AppState;
use crate::utils::leaderboard_relay::build_local_stats_bundle;

#[utoipa::path(
    get,
    path = "/api/public-stats-bundle",
    responses(
        (status = 200, description = "Public leaderboard bundle for this library")
    )
)]
pub async fn get_public_stats_bundle(State(state): State<AppState>) -> Json<Value> {
    Json(build_local_stats_bundle(&state).await)
}
