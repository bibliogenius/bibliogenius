// Library name flash-editor updater.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Library Name ============

/// Update only the library name in the database (library_config + libraries tables).
/// This is the FFI-direct path used by the flash editor on the home screen.
/// Only touches the `name` and `updated_at` fields - no other settings are overwritten.
pub async fn update_library_name_ffi(name: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;

    use crate::models::library_config;
    use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel, Set};

    // Update library_config.name (id=1)
    let config = library_config::Entity::find_by_id(1)
        .one(db)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(c) = config {
        let mut active = c.into_active_model();
        active.name = Set(name.clone());
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(db).await.map_err(|e| e.to_string())?;
    }

    // Also update libraries.name (id=1) for consistency
    use crate::models::library;

    let lib = library::Entity::find_by_id(1)
        .one(db)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(l) = lib {
        let mut active = l.into_active_model();
        active.name = Set(name);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(db).await.map_err(|e| e.to_string())?;
    }

    Ok(())
}
