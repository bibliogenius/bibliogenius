use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde::Deserialize;
use serde_json::json;

use crate::models::installation_profile::{ActiveModel, Entity as InstallationProfileEntity};

#[derive(Debug, Deserialize)]
pub struct UpdateProfileRequest {
    #[serde(default)]
    pub profile_type: Option<String>,
    #[serde(default)]
    pub avatar_config: Option<serde_json::Value>,
    #[serde(default)]
    pub fallback_preferences: Option<std::collections::HashMap<String, bool>>,
    #[serde(default)]
    pub enabled_modules: Option<Vec<String>>,
}

pub async fn update_profile(
    State(db): State<DatabaseConnection>,
    Json(req): Json<UpdateProfileRequest>,
) -> impl IntoResponse {
    // Validate profile type if provided
    if let Some(ref profile_type) = req.profile_type
        && profile_type != "individual"
        && profile_type != "professional"
        && profile_type != "librarian"
        && profile_type != "kid"
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Invalid profile type. Must be 'individual', 'professional', 'librarian' or 'kid'"})),
        )
            .into_response();
    }

    // Find existing profile (assume ID 1)
    let profile = InstallationProfileEntity::find_by_id(1)
        .one(&db)
        .await
        .unwrap_or(None);

    if let Some(existing_profile) = profile {
        let mut active: ActiveModel = existing_profile.clone().into();

        if let Some(ref profile_type) = req.profile_type {
            active.profile_type = Set(profile_type.clone());
        }

        if let Some(avatar_config) = req.avatar_config {
            active.avatar_config = Set(Some(
                serde_json::to_string(&avatar_config).unwrap_or_default(),
            ));
        }

        // Handle direct enabled_modules update
        if let Some(ref modules) = req.enabled_modules {
            active.enabled_modules = Set(serde_json::to_string(modules).unwrap_or_default());
        }

        // Handle fallback preferences (toggle-based module flags)
        if let Some(prefs) = req.fallback_preferences {
            let mut modules: Vec<String> = if req.enabled_modules.is_some() {
                // If enabled_modules was also set, use that as the base
                req.enabled_modules.clone().unwrap_or_default()
            } else {
                serde_json::from_str(&existing_profile.enabled_modules).unwrap_or_default()
            };

            for (provider, enabled) in prefs {
                if provider == "google_books" {
                    let enable_flag = "enable_google_books".to_string();
                    if enabled {
                        if !modules.contains(&enable_flag) {
                            modules.push(enable_flag);
                        }
                    } else {
                        modules.retain(|m| m != &enable_flag);
                    }
                } else {
                    let disable_flag = format!("disable_fallback:{}", provider);
                    if enabled {
                        // Remove disable flag
                        modules.retain(|m| m != &disable_flag);
                    } else {
                        // Add disable flag if not present
                        if !modules.contains(&disable_flag) {
                            modules.push(disable_flag);
                        }
                    }
                }
            }
            active.enabled_modules = Set(serde_json::to_string(&modules).unwrap_or_default());
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
        use crate::models::library_config::{
            ActiveModel as ConfigActiveModel, Entity as ConfigEntity,
        };

        if let Some(ref profile_type) = req.profile_type
            && let Ok(Some(config)) = ConfigEntity::find_by_id(1).one(&db).await
        {
            let mut active_config: ConfigActiveModel = config.into();
            active_config.show_borrowed_books = Set(Some(profile_type == "individual"));
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
