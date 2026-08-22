// Favorites typed collection (ADR-064): toggle, membership list, seeding,
// one-shot adoption. Everything delegates to services::favorites_service.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Favorites ────────────────────────────────────────────────────────

/// Toggle a book's favorite state (membership in the `source = 'favorites'`
/// collection). Returns the NEW state (`true` = now a favorite). The
/// collection is created lazily on the first marking and duplicates from
/// other devices are merged first (keep-oldest rule, ADR-064).
pub async fn toggle_favorite_book(book_id: String) -> Result<bool, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::favorites_service::toggle_favorite_book(db, &book_id)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// All favorite book ids in one pass. Cached Flutter-side (provider), so
/// cards never call this per item. Empty when no favorites collection
/// exists; reading never creates it.
pub async fn get_favorite_book_ids() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::favorites_service::get_favorite_book_ids(db)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Seed the empty favorites collection at Reader-profile selection. The
/// eligibility gate (no typed collection, no favorites-like collection
/// name, no favorites-like shelf label) is enforced Rust-side; returns
/// `true` only when the collection was actually created. Never call this
/// from startup or migration paths (ADR-064).
pub async fn seed_favorites_collection() -> Result<bool, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::favorites_service::seed_favorites_collection(db)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// The manual collection to propose for one-shot adoption on the first
/// favorite tap (oldest favorites-like `source = 'manual'` collection), or
/// None. The remembered refusal is device-local, Flutter-side.
pub async fn get_favorites_adoption_candidate() -> Result<Option<FrbCollection>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::favorites_service::get_favorites_adoption_candidate(db)
        .await
        .map(|opt| opt.map(FrbCollection::from))
        .map_err(|e| format!("{e:?}"))
}

/// Adopt a manual collection as THE favorites collection: flips its source
/// to 'favorites', keeps its name and members.
pub async fn adopt_favorites_collection(collection_id: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::favorites_service::adopt_favorites_collection(db, &collection_id)
        .await
        .map_err(|e| format!("{e:?}"))
}
