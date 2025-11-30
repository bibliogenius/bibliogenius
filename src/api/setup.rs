use crate::models::{installation_profile, library_config};
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub profile_type: String, // "individual" or "professional"
    pub library_name: String,
    pub library_description: Option<String>,
    pub theme: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub share_location: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct SetupResponse {
    pub success: bool,
    pub message: String,
}

pub async fn setup(
    State(db): State<DatabaseConnection>,
    Json(req): Json<SetupRequest>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    // Update or create installation profile
    let profile = installation_profile::ActiveModel {
        id: Set(1),
        profile_type: Set(req.profile_type.clone()),
        enabled_modules: Set("[]".to_string()), // Start with no modules
        theme: Set(req.theme.or(Some("default".to_string()))),
        updated_at: Set(now.to_rfc3339()),
        created_at: Set(now.to_rfc3339()),
        ..Default::default()
    };

    if let Err(e) = profile.save(&db).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SetupResponse {
                success: false,
                message: format!("Failed to save profile: {}", e),
            }),
        )
            .into_response();
    }

    // Update or create library config
    let config = library_config::ActiveModel {
        id: Set(1),
        name: Set(req.library_name.clone()),
        description: Set(req.library_description.clone()),
        tags: Set("[]".to_string()),
        latitude: Set(req.latitude),
        longitude: Set(req.longitude),
        share_location: Set(req.share_location.or(Some(false))),
        show_borrowed_books: Set(Some(req.profile_type == "individual")),
        updated_at: Set(now.to_rfc3339()),
        created_at: Set(now.to_rfc3339()),
        ..Default::default()
    };

    if let Err(e) = config.save(&db).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SetupResponse {
                success: false,
                message: format!("Failed to save library config: {}", e),
            }),
        )
            .into_response();
    }

    // Create admin user if not exists
    use crate::models::user;
    use crate::auth::hash_password;

    let admin_exists = user::Entity::find()
        .filter(user::Column::Username.eq("admin"))
        .one(&db)
        .await
        .unwrap_or(None);

    if admin_exists.is_none() {
        println!("Admin user not found, creating...");
        let password_hash = hash_password("admin").unwrap();
        let admin = user::ActiveModel {
            username: Set("admin".to_string()),
            password_hash: Set(password_hash),
            role: Set("admin".to_string()),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
            ..Default::default()
        };
        
        if let Err(e) = admin.insert(&db).await {
             println!("Failed to create admin user: {}", e);
             return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SetupResponse {
                    success: false,
                    message: format!("Failed to create admin user: {}", e),
                }),
            )
                .into_response();
        }
        println!("Admin user created successfully");
    } else {
        println!("Admin user already exists");
    }

    // Create default library if not exists (Required for copies)
    use crate::models::library;
    let admin_user = user::Entity::find()
        .filter(user::Column::Username.eq("admin"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let library_exists = library::Entity::find_by_id(1).one(&db).await.unwrap_or(None);
    if library_exists.is_none() {
        println!("Default library not found, creating...");
        let new_library = library::ActiveModel {
            id: Set(1),
            name: Set(req.library_name.clone()),
            description: Set(req.library_description.clone()),
            owner_id: Set(admin_user.id),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
            ..Default::default()
        };
        if let Err(e) = new_library.insert(&db).await {
            println!("Failed to create default library: {}", e);
        } else {
            println!("Default library created successfully");
        }
    }

    (
        StatusCode::OK,
        Json(SetupResponse {
            success: true,
            message: "Setup completed successfully".to_string(),
        }),
    )
        .into_response()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConfigResponse {
    pub id: i32,
    pub library_name: String,
    pub library_description: Option<String>,
    pub profile_type: String,
    pub enabled_modules: Vec<String>,
    pub theme: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub share_location: bool,
    pub show_borrowed_books: bool,
}

pub async fn get_config(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::{installation_profile, library_config};

    let config = match library_config::Entity::find_by_id(1).one(&db).await {
        Ok(Some(c)) => c,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Config not found"})),
            )
                .into_response()
        }
    };

    let profile = match installation_profile::Entity::find_by_id(1).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Profile not found"})),
            )
                .into_response()
        }
    };

    let enabled_modules: Vec<String> =
        serde_json::from_str(&profile.enabled_modules).unwrap_or_default();

    (
        StatusCode::OK,
        Json(ConfigResponse {
            id: config.id,
            library_name: config.name,
            library_description: config.description,
            profile_type: profile.profile_type.clone(),
            enabled_modules,
            theme: profile.theme.unwrap_or_else(|| "default".to_string()),
            latitude: if profile.profile_type == "individual" {
                config.latitude.map(|l| (l * 100.0).round() / 100.0) // Round to 2 decimal places (~1.1km)
            } else {
                config.latitude
            },
            longitude: if profile.profile_type == "individual" {
                config.longitude.map(|l| (l * 100.0).round() / 100.0)
            } else {
                config.longitude
            },
            share_location: config.share_location.unwrap_or(false),
            show_borrowed_books: config.show_borrowed_books.unwrap_or(false),
        }),
    )
        .into_response()
}
