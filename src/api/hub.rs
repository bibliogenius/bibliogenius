use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::peer::{get_safe_client, validate_url};
use crate::infrastructure::AppState;
use crate::models::LibraryConfig;
use crate::models::library_config::Entity as LibraryConfigEntity;
use sea_orm::EntityTrait;

#[derive(Debug, Serialize, Deserialize)]
pub struct RegistrationRequest {
    pub hub_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HubRegistration {
    pub library_name: String,
    pub url: String,
    pub tags: Vec<String>,
    pub description: Option<String>,
}

pub async fn register_with_hub(
    State(state): State<AppState>,
    Json(req): Json<RegistrationRequest>,
) -> Result<Json<Value>, StatusCode> {
    let db = state.db();

    // Get library config
    let config = LibraryConfigEntity::find()
        .one(db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let library_config = config
        .map(LibraryConfig::from)
        .unwrap_or_else(|| LibraryConfig {
            name: "My Library".to_string(),
            description: None,
            tags: vec![],
            latitude: None,
            longitude: None,
            share_location: false,
            show_borrowed_books: false,
        });

    // Prepare registration data
    let registration = HubRegistration {
        library_name: library_config.name,
        url: state.our_public_url(),
        tags: library_config.tags,
        description: library_config.description,
    };

    // Validate hub URL to prevent SSRF (OWASP A10)
    let hub_url = validate_url(&req.hub_url).map_err(|_| StatusCode::BAD_REQUEST)?;

    // Send registration to hub
    let client = get_safe_client();
    let response = client
        .post(format!(
            "{}/api/registry/register",
            hub_url.trim_end_matches('/')
        ))
        .json(&registration)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if response.status().is_success() {
        Ok(Json(json!({
            "message": "Successfully registered with hub",
            "hub_url": req.hub_url
        })))
    } else {
        Err(StatusCode::BAD_GATEWAY)
    }
}

pub async fn discover_peers(
    State(state): State<AppState>,
    Json(req): Json<RegistrationRequest>,
) -> Result<Json<Value>, StatusCode> {
    let _db = state.db();

    // Validate hub URL to prevent SSRF (OWASP A10)
    let hub_url = validate_url(&req.hub_url).map_err(|_| StatusCode::BAD_REQUEST)?;

    // Query hub for peers
    let client = get_safe_client();
    let response = client
        .get(format!(
            "{}/api/discovery/peers",
            hub_url.trim_end_matches('/')
        ))
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if response.status().is_success() {
        let peers: Value = response
            .json()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        Ok(Json(peers))
    } else {
        Err(StatusCode::BAD_GATEWAY)
    }
}
