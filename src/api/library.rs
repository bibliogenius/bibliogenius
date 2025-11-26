use axum::{extract::State, http::StatusCode, Json};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{json, Value};

use crate::models::library_config::{ActiveModel, Entity as LibraryConfigEntity};
use crate::models::LibraryConfig;

pub async fn get_config(State(db): State<DatabaseConnection>) -> Result<Json<Value>, StatusCode> {
    // Get the first (and only) library config
    let config = LibraryConfigEntity::find()
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match config {
        Some(cfg) => Ok(Json(json!(LibraryConfig::from(cfg)))),
        None => Ok(Json(json!({
            "name": "My Library",
            "description": null,
            "tags": []
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
        active.updated_at = Set(now.into());

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
            created_at: Set(now.into()),
            updated_at: Set(now.into()),
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
