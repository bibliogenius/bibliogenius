use crate::models::{installation_profile, library_config};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
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
    pub admin_username: Option<String>,
    pub admin_password: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SetupResponse {
    pub success: bool,
    pub message: String,
    pub user_id: Option<i32>,
    pub library_id: Option<i32>,
}

pub async fn setup(
    State(db): State<DatabaseConnection>,
    Json(req): Json<SetupRequest>,
) -> impl IntoResponse {
    let now = chrono::Utc::now();

    // Update or create installation profile using insert with on_conflict
    let profile = installation_profile::ActiveModel {
        id: Set(1),
        profile_type: Set(req.profile_type.clone()),
        enabled_modules: Set("[]".to_string()), // Start with no modules
        theme: Set(req.theme.clone().or(Some("default".to_string()))),
        avatar_config: Set(None),
        api_keys: Set(None),
        updated_at: Set(now.to_rfc3339()),
        created_at: Set(now.to_rfc3339()),
    };

    if let Err(e) = installation_profile::Entity::insert(profile)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(installation_profile::Column::Id)
                .update_columns([
                    installation_profile::Column::ProfileType,
                    installation_profile::Column::EnabledModules,
                    installation_profile::Column::Theme,
                    installation_profile::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(&db)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SetupResponse {
                success: false,
                message: format!("Failed to save profile: {}", e),
                user_id: None,
                library_id: None,
            }),
        )
            .into_response();
    }

    // Update or create library config using insert with on_conflict
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

    if let Err(e) = library_config::Entity::insert(config)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(library_config::Column::Id)
                .update_columns([
                    library_config::Column::Name,
                    library_config::Column::Description,
                    library_config::Column::Latitude,
                    library_config::Column::Longitude,
                    library_config::Column::ShareLocation,
                    library_config::Column::ShowBorrowedBooks,
                    library_config::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(&db)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SetupResponse {
                success: false,
                message: format!("Failed to save library config: {}", e),
                user_id: None,
                library_id: None,
            }),
        )
            .into_response();
    }

    // Create admin user if not exists (using raw SQL to avoid totp_secret column issue)
    use crate::auth::hash_password;
    use crate::models::user;
    use sea_orm::ConnectionTrait;

    // Get username and password from request, with defaults for backward compatibility
    let admin_username = req
        .admin_username
        .clone()
        .unwrap_or_else(|| "admin".to_string());
    let admin_password = req
        .admin_password
        .clone()
        .unwrap_or_else(|| "admin".to_string());

    // Check if user with this username already exists
    let admin_exists = match db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "SELECT COUNT(*) FROM users WHERE username = '{}'",
                admin_username
            ),
        ))
        .await
    {
        Ok(Some(row)) => row.try_get_by_index::<i32>(0).unwrap_or(0) > 0,
        _ => false,
    };

    if !admin_exists {
        tracing::info!("Admin user '{}' not found, creating...", admin_username);
        let password_hash = hash_password(&admin_password).unwrap();
        let admin = user::ActiveModel {
            username: Set(admin_username.clone()),
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
                    user_id: None,
                    library_id: None,
                }),
            )
                .into_response();
        }
        tracing::info!("Admin user '{}' created successfully", admin_username);
    } else {
        tracing::info!("Admin user '{}' already exists", admin_username);
    }

    // Create default library using on_conflict (Required for copies)
    use crate::models::library;

    // Get admin user ID using raw query to avoid totp_secret column issue
    let admin_user_id: Option<i32> = match db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "SELECT id FROM users WHERE username = '{}' LIMIT 1",
                admin_username
            ),
        ))
        .await
    {
        Ok(Some(row)) => row.try_get_by_index::<i32>(0).ok(),
        _ => None,
    };

    let admin_id = match admin_user_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SetupResponse {
                    success: false,
                    message: "Admin user not found after creation".to_string(),
                    user_id: None,
                    library_id: None,
                }),
            )
                .into_response();
        }
    };

    let new_library = library::ActiveModel {
        id: Set(1),
        name: Set(req.library_name.clone()),
        description: Set(req.library_description.clone()),
        owner_id: Set(admin_id),
        created_at: Set(now.to_rfc3339()),
        updated_at: Set(now.to_rfc3339()),
    };

    match library::Entity::insert(new_library)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(library::Column::Id)
                .update_columns([
                    library::Column::Name,
                    library::Column::Description,
                    library::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(&db)
        .await
    {
        Err(e) => {
            tracing::error!("Failed to create default library: {}", e);
        }
        _ => {
            tracing::info!("Default library created/updated successfully");
        }
    }

    (
        StatusCode::OK,
        Json(SetupResponse {
            success: true,
            message: "Setup completed successfully".to_string(),
            user_id: Some(admin_id),
            library_id: Some(1),
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
    /// Whether this library allows others to cache its catalog for offline viewing
    #[serde(default)]
    pub allow_library_caching: bool,
    /// Whether this library shares gamification stats with peers
    #[serde(default)]
    pub share_gamification_stats: bool,
    /// Stable library UUID for P2P peer deduplication (survives IP/port changes)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_uuid: Option<String>,
    /// Ed25519 public key (hex-encoded) for E2EE signature verification
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ed25519_public_key: Option<String>,
    /// X25519 public key (hex-encoded) for E2EE key exchange
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x25519_public_key: Option<String>,
    /// Relay hub URL (if relay is configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    /// Relay mailbox UUID (if relay is configured)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox_id: Option<String>,
    /// Write token for peers to deposit relay messages
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_write_token: Option<String>,
}

pub async fn get_config(State(state): State<crate::infrastructure::AppState>) -> impl IntoResponse {
    use crate::models::{installation_profile, library_config};

    let db = state.db();
    let config = library_config::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten();

    let profile = installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
        .ok()
        .flatten();

    let (profile_type, enabled_modules, theme) = match &profile {
        Some(p) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            (
                p.profile_type.clone(),
                modules,
                p.theme.clone().unwrap_or_else(|| "default".to_string()),
            )
        }
        None => ("individual".to_string(), vec![], "default".to_string()),
    };

    // Check if library owner allows caching (opt-in, default false for privacy)
    let allow_library_caching = enabled_modules.contains(&"allow_library_caching".to_string());
    let share_gamification_stats =
        enabled_modules.contains(&"share_gamification_stats".to_string());

    // Load E2EE public keys from crypto_keys table (if identity has been initialized)
    let (ed25519_public_key, x25519_public_key) = load_public_keys_from_db(db).await;

    // Load relay config (if configured)
    let relay_config = crate::api::relay::get_my_relay_config(db).await;

    // Get our library UUID from the identity service (stable P2P identifier)
    let library_uuid = state.identity_service.library_uuid().map(|s| s.to_string());

    let (
        id,
        library_name,
        library_description,
        latitude,
        longitude,
        share_location,
        show_borrowed_books,
    ) = match &config {
        Some(c) => (
            c.id,
            c.name.clone(),
            c.description.clone(),
            if profile_type == "individual" {
                c.latitude.map(|l| (l * 100.0).round() / 100.0)
            } else {
                c.latitude
            },
            if profile_type == "individual" {
                c.longitude.map(|l| (l * 100.0).round() / 100.0)
            } else {
                c.longitude
            },
            c.share_location.unwrap_or(false),
            c.show_borrowed_books.unwrap_or(false),
        ),
        None => (0, "My Library".to_string(), None, None, None, false, false),
    };

    (
        StatusCode::OK,
        Json(ConfigResponse {
            id,
            library_name,
            library_description,
            profile_type,
            enabled_modules,
            theme,
            latitude,
            longitude,
            share_location,
            show_borrowed_books,
            allow_library_caching,
            share_gamification_stats,
            library_uuid,
            ed25519_public_key,
            x25519_public_key,
            relay_url: relay_config.as_ref().map(|r| r.relay_url.clone()),
            mailbox_id: relay_config.as_ref().map(|r| r.mailbox_uuid.clone()),
            relay_write_token: relay_config.as_ref().map(|r| r.write_token.clone()),
        }),
    )
        .into_response()
}

/// Load hex-encoded public keys from crypto_keys table.
/// Returns (ed25519_hex, x25519_hex), both Option.
pub async fn load_public_keys_from_db(db: &DatabaseConnection) -> (Option<String>, Option<String>) {
    use sea_orm::ConnectionTrait;

    let rows = match db
        .query_all(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "SELECT key_type, public_key FROM crypto_keys WHERE user_id = 0 AND revoked_at IS NULL"
                .to_owned(),
        ))
        .await
    {
        Ok(rows) => rows,
        Err(_) => return (None, None),
    };

    let mut ed25519: Option<String> = None;
    let mut x25519: Option<String> = None;

    for row in &rows {
        let key_type: String = match row.try_get("", "key_type") {
            Ok(v) => v,
            Err(_) => continue,
        };
        let public_key: Vec<u8> = match row.try_get("", "public_key") {
            Ok(v) => v,
            Err(_) => continue,
        };

        match key_type.as_str() {
            "ed25519" => ed25519 = Some(hex::encode(&public_key)),
            "x25519" => x25519 = Some(hex::encode(&public_key)),
            _ => {}
        }
    }

    (ed25519, x25519)
}

/// POST /api/identity/init — Initialize the node's E2EE identity (desktop/test mode).
/// In FFI mode, Flutter calls init_identity_ffi() instead.
pub async fn init_identity(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<InitIdentityRequest>,
) -> impl IntoResponse {
    match state.identity_service.init(&payload.library_uuid).await {
        Ok(()) => {
            // Trigger lazy init of CryptoService
            let _ = state.crypto_service();
            let (ed25519, x25519) = load_public_keys_from_db(state.db()).await;
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Identity initialized",
                    "ed25519_public_key": ed25519,
                    "x25519_public_key": x25519,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to initialize identity: {e}") })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct InitIdentityRequest {
    pub library_uuid: String,
}

pub async fn reset_app(
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
) -> impl IntoResponse {
    use crate::models::{
        author, book, book_authors, book_tags, collection, collection_book, contact, copy,
        installation_profile, library, library_config, loan, operation_log, p2p_outgoing_request,
        p2p_request, peer, peer_book, peer_gamification_stats, tag, user,
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
    delete_all!(collection_book);
    delete_all!(collection);
    delete_all!(book_authors);
    delete_all!(book_tags);
    delete_all!(book);
    delete_all!(author);
    delete_all!(tag);

    delete_all!(p2p_outgoing_request);
    delete_all!(p2p_request);
    delete_all!(peer_book);
    delete_all!(peer_gamification_stats);
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
