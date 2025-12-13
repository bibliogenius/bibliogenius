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
        avatar_config: Set(None),
        updated_at: Set(now.to_rfc3339()),
        created_at: Set(now.to_rfc3339()),
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
    use crate::auth::hash_password;
    use crate::models::user;

    let admin_exists = user::Entity::find()
        .filter(user::Column::Username.eq("admin"))
        .one(&db)
        .await
        .unwrap_or(None);

    if admin_exists.is_none() {
        tracing::info!("Admin user not found, creating...");
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
            tracing::error!("Failed to create admin user: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SetupResponse {
                    success: false,
                    message: format!("Failed to create admin user: {}", e),
                }),
            )
                .into_response();
        }
        tracing::info!("Admin user created successfully");
    } else {
        tracing::info!("Admin user already exists");
    }

    // Create default library if not exists (Required for copies)
    use crate::models::library;
    let admin_user = user::Entity::find()
        .filter(user::Column::Username.eq("admin"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let library_exists = library::Entity::find_by_id(1)
        .one(&db)
        .await
        .unwrap_or(None);
    if library_exists.is_none() {
        tracing::info!("Default library not found, creating...");
        let new_library = library::ActiveModel {
            id: Set(1),
            name: Set(req.library_name.clone()),
            description: Set(req.library_description.clone()),
            owner_id: Set(admin_user.id),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
        };
        if let Err(e) = new_library.insert(&db).await {
            tracing::error!("Failed to create default library: {}", e);
        } else {
            tracing::info!("Default library created successfully");
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

pub async fn reset_app(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::{
        author, book, book_authors, book_tags, contact, copy, installation_profile, library,
        library_config, loan, operation_log, p2p_outgoing_request, p2p_request, peer, peer_book,
        tag, user,
    };

    // Helper macro to delete all from a table
    macro_rules! delete_all {
        ($entity:ident) => {
            if let Err(e) = $entity::Entity::delete_many().exec(&db).await {
                tracing::error!("Failed to delete from {}: {}", stringify!($entity), e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("Failed to delete {}: {}", stringify!($entity), e)})),
                )
                    .into_response();
            }
        };
    }

    // Delete in order of dependencies (child tables first)
    delete_all!(loan);
    delete_all!(copy);
    delete_all!(book_authors);
    delete_all!(book_tags);
    delete_all!(book);
    delete_all!(author);
    delete_all!(tag);

    delete_all!(p2p_outgoing_request);
    delete_all!(p2p_request);
    delete_all!(peer_book);
    delete_all!(peer);
    delete_all!(contact);

    delete_all!(operation_log);

    delete_all!(library_config);
    delete_all!(library);
    delete_all!(installation_profile);

    // We keep the admin user for now, or we could delete it too.
    // If we delete it, the setup process will recreate it.
    // Let's delete everything to be safe and clean.
    delete_all!(user);

    tracing::info!("App reset successful: All data cleared.");

    (
        StatusCode::OK,
        Json(json!({"success": true, "message": "App reset successfully"})),
    )
        .into_response()
}
