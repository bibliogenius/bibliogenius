//! Shared helper for resolving library_id dynamically.
//!
//! Many tables (contacts, copies, loans, sales) have FK references to libraries(id).
//! This helper finds the first library or creates a default one if none exists.

use sea_orm::{ActiveModelTrait, DbErr, EntityTrait, Set};

use crate::models::{library, user};

/// Resolve the library ID: return the first library's ID, or create one if none exists.
///
/// This is the single source of truth for "which library does the local user own?".
/// Never hardcode library_id = 1 - always call this instead.
///
/// Bootstraps both user and library if neither exists (handles fresh DB or
/// FFI mode where the setup wizard may not have run yet).
pub async fn resolve_library_id<C: sea_orm::ConnectionTrait>(db: &C) -> Result<i32, DbErr> {
    // Fast path: a library already exists
    if let Some(lib) = library::Entity::find().one(db).await? {
        return Ok(lib.id);
    }

    // No library exists - find or create a user first
    let owner = match user::Entity::find().one(db).await? {
        Some(u) => u,
        None => {
            // No user at all (fresh DB, pre-setup). Bootstrap a default admin.
            let now = chrono::Utc::now().to_rfc3339();
            let default_hash =
                crate::auth::hash_password("admin").unwrap_or_else(|_| "!locked".to_string());
            let admin = user::ActiveModel {
                username: Set("admin".to_string()),
                password_hash: Set(default_hash),
                role: Set("admin".to_string()),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            let created_user = admin.insert(db).await?;
            tracing::info!("Auto-created default admin user (id={})", created_user.id);
            created_user
        }
    };

    let now = chrono::Utc::now().to_rfc3339();
    let new_library = library::ActiveModel {
        name: Set("My Library".to_string()),
        description: Set(None),
        owner_id: Set(owner.id),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let created = new_library.insert(db).await?;
    tracing::info!(
        "Auto-created default library (id={}) for user {}",
        created.id,
        owner.id
    );
    Ok(created.id)
}
