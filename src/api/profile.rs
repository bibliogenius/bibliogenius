use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde::Deserialize;
use serde_json::json;

use crate::models::installation_profile::{ActiveModel, Entity as InstallationProfileEntity};

#[derive(Debug, Deserialize)]
pub struct UpdateProfileRequest {
    pub profile_type: String,
    pub avatar_config: Option<serde_json::Value>,
}

pub async fn update_profile(
    State(db): State<DatabaseConnection>,
    Json(req): Json<UpdateProfileRequest>,
) -> impl IntoResponse {
    // Validate profile type
    if req.profile_type != "individual" && req.profile_type != "professional" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid profile type. Must be 'individual' or 'professional'"})),
        )
            .into_response();
    }

    // Find existing profile (assume ID 1)
    let profile = InstallationProfileEntity::find_by_id(1)
        .one(&db)
        .await
        .unwrap_or(None);

    if let Some(existing_profile) = profile {
        let mut active: ActiveModel = existing_profile.into();
        active.profile_type = Set(req.profile_type.clone());

        if let Some(avatar_config) = req.avatar_config {
            active.avatar_config = Set(Some(
                serde_json::to_string(&avatar_config).unwrap_or_default(),
            ));
        }

        active.updated_at = Set(chrono::Utc::now().to_rfc3339());

        if let Err(e) = active.update(&db).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to update profile: {}", e)})),
            )
                .into_response();
        }

        // Also update library config defaults based on profile type
        // If switching to individual -> show_borrowed_books = true
        // If switching to professional -> show_borrowed_books = false
        // We need to access library_config model here.
        use crate::models::library_config::{
            ActiveModel as ConfigActiveModel, Entity as ConfigEntity,
        };

        if let Ok(Some(config)) = ConfigEntity::find_by_id(1).one(&db).await {
            let mut active_config: ConfigActiveModel = config.into();
            active_config.show_borrowed_books = Set(Some(req.profile_type == "individual"));
            let _ = active_config.update(&db).await;
        }

        (
            StatusCode::OK,
            Json(json!({
                "message": "Profile updated successfully",
                "profile_type": req.profile_type
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Installation profile not found"})),
        )
            .into_response()
    }
}
