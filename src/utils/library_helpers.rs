//! Shared helper for resolving library_id dynamically.
//!
//! Many tables (contacts, copies, loans, sales) have FK references to libraries(id).
//! This helper finds the first library or creates a default one if none exists.

use sea_orm::{ActiveModelTrait, DbErr, EntityTrait, Set};

use crate::models::{library, library_config, user};

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

/// Resolve the user-facing display name of the local library for outgoing
/// P2P loan payloads (loan_offer / loan_accept / relay loan confirmation).
///
/// The authoritative source is `library_config.name`: the singleton row the
/// setup wizard writes, used everywhere else (mDNS, hub, E2EE identity, export,
/// leaderboard). The older `library` table is a FK target with a hardcoded
/// seed ("My Library") and must not be treated as authoritative.
///
/// Falls back in order:
/// 1. `library_config.name` (non-empty)
/// 2. `library.name` (non-empty, first row — never hardcode id=1)
/// 3. `"Unknown Library"`
pub async fn resolve_lender_display_name<C: sea_orm::ConnectionTrait>(db: &C) -> String {
    if let Ok(Some(cfg)) = library_config::Entity::find().one(db).await
        && !cfg.name.trim().is_empty()
    {
        return cfg.name;
    }
    if let Ok(Some(lib)) = library::Entity::find().one(db).await
        && !lib.name.trim().is_empty()
    {
        return lib.name;
    }
    "Unknown Library".to_string()
}

#[cfg(test)]
mod tests {
    //! Migrations seed `library_config` with `(1, 'My Library', ...)` unconditionally
    //! and `libraries` only when user id=1 exists. Tests explicitly control both
    //! tables rather than relying on the seed.
    use super::*;
    use chrono::Utc;
    use sea_orm::{Database, DatabaseConnection, ModelTrait};

    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db)
            .await
            .unwrap();
        db
    }

    async fn set_library_config_name(db: &DatabaseConnection, name: Option<&str>) {
        let existing = library_config::Entity::find().one(db).await.unwrap();
        match (existing, name) {
            (Some(row), Some(n)) => {
                let mut active: library_config::ActiveModel = row.into();
                active.name = Set(n.to_string());
                active.updated_at = Set(Utc::now().to_rfc3339());
                active.update(db).await.unwrap();
            }
            (Some(row), None) => {
                row.delete(db).await.unwrap();
            }
            (None, Some(n)) => {
                library_config::ActiveModel {
                    name: Set(n.to_string()),
                    description: Set(None),
                    tags: Set("[]".to_string()),
                    latitude: Set(None),
                    longitude: Set(None),
                    share_location: Set(Some(false)),
                    show_borrowed_books: Set(Some(false)),
                    created_at: Set(Utc::now().to_rfc3339()),
                    updated_at: Set(Utc::now().to_rfc3339()),
                    ..Default::default()
                }
                .insert(db)
                .await
                .unwrap();
            }
            (None, None) => {}
        }
    }

    async fn insert_library(db: &DatabaseConnection, name: &str) {
        let owner = user::ActiveModel {
            username: Set("alice".to_string()),
            password_hash: Set("!locked".to_string()),
            role: Set("admin".to_string()),
            created_at: Set(Utc::now().to_rfc3339()),
            updated_at: Set(Utc::now().to_rfc3339()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();

        library::ActiveModel {
            name: Set(name.to_string()),
            description: Set(None),
            owner_id: Set(owner.id),
            created_at: Set(Utc::now().to_rfc3339()),
            updated_at: Set(Utc::now().to_rfc3339()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prefers_library_config_name() {
        let db = test_db().await;
        insert_library(&db, "Legacy Seed").await;
        set_library_config_name(&db, Some("Alice's Library")).await;

        assert_eq!(resolve_lender_display_name(&db).await, "Alice's Library");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn falls_back_to_library_when_config_missing() {
        let db = test_db().await;
        set_library_config_name(&db, None).await;
        insert_library(&db, "Fallback Name").await;

        assert_eq!(resolve_lender_display_name(&db).await, "Fallback Name");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn final_fallback_when_both_missing() {
        let db = test_db().await;
        set_library_config_name(&db, None).await;

        assert_eq!(resolve_lender_display_name(&db).await, "Unknown Library");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ignores_blank_library_config_name() {
        let db = test_db().await;
        set_library_config_name(&db, Some("   ")).await;
        insert_library(&db, "Fallback Name").await;

        assert_eq!(resolve_lender_display_name(&db).await, "Fallback Name");
    }
}
