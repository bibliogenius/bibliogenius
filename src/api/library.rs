use axum::{Json, extract::State, http::StatusCode};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{Value, json};

use crate::models::LibraryConfig;
use crate::models::library_config::{ActiveModel, Entity as LibraryConfigEntity};

pub async fn get_config(State(db): State<DatabaseConnection>) -> Result<Json<Value>, StatusCode> {
    // Get the first (and only) library config
    let config = LibraryConfigEntity::find()
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let enabled_modules = match crate::models::installation_profile::Entity::find_by_id(1)
        .one(&db)
        .await
    {
        Ok(Some(profile)) => {
            serde_json::from_str::<Vec<String>>(&profile.enabled_modules).unwrap_or_default()
        }
        _ => {
            vec![]
        }
    };

    match config {
        Some(cfg) => {
            let mut json_val = serde_json::to_value(LibraryConfig::from(cfg))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            if let Some(obj) = json_val.as_object_mut() {
                obj.insert("enabled_modules".to_string(), json!(enabled_modules));
                // Also add profile_type for convenience if passing it here
                if let Ok(Some(profile)) =
                    crate::models::installation_profile::Entity::find_by_id(1)
                        .one(&db)
                        .await
                {
                    obj.insert("profile_type".to_string(), json!(profile.profile_type));
                }
            }
            Ok(Json(json_val))
        }
        None => Ok(Json(json!({
            "name": "My Library",
            "description": null,
            "tags": [],
            "enabled_modules": enabled_modules
        }))),
    }
}

pub async fn update_config(
    State(db): State<DatabaseConnection>,
    Json(config): Json<LibraryConfig>,
) -> Result<Json<Value>, StatusCode> {
    let now = chrono::Utc::now();
    let tags_json = serde_json::to_string(&config.tags).unwrap_or_else(|_| "[]".to_string());

    // Check if config exists
    let existing = LibraryConfigEntity::find()
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some(existing_config) = existing {
        // Update existing
        let mut active: ActiveModel = existing_config.into();
        active.name = Set(config.name);
        active.description = Set(config.description);
        active.tags = Set(tags_json);
        active.share_location = Set(Some(config.share_location));
        active.show_borrowed_books = Set(Some(config.show_borrowed_books));
        active.updated_at = Set(now.to_rfc3339());

        active
            .update(&db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    } else {
        // Create new
        let new_config = ActiveModel {
            name: Set(config.name),
            description: Set(config.description),
            tags: Set(tags_json),
            share_location: Set(Some(config.share_location)),
            show_borrowed_books: Set(Some(config.show_borrowed_books)),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
            ..Default::default()
        };

        new_config
            .insert(&db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    Ok(Json(json!({
        "message": "Library configuration updated successfully"
    })))
}
