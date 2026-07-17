// Installation profile search settings and fallback preferences.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Installation profile (search settings via FFI direct) ────────────

/// Search-related settings persisted in the installation profile.
///
/// Mirrors the `fallback_preferences` and `api_keys` fields the Flutter
/// settings screen expects under `_userStatus['config']`. The legacy
/// `gamification_get_status` payload (returned by `getUserStatus` in FFI
/// mode) does not carry these fields, hence this dedicated accessor.
pub struct FrbSearchSettings {
    /// Per-provider toggles. Only entries the user explicitly toggled away
    /// from the implicit defaults are populated, so the client's `?? default`
    /// fallbacks (e.g. `bnfDefault = isFrench`) keep applying for untouched
    /// providers — preventing UI regressions on the BNF locale heuristic.
    ///
    /// Mapping (inverse of `api/profile.rs::update_profile`):
    /// - `enable_google_books` in modules → `google_books: true`
    /// - `disable_fallback:<provider>` in modules → `<provider>: false`
    pub fallback_preferences: std::collections::HashMap<String, bool>,
    /// API keys stored in `installation_profile.api_keys`
    /// (e.g. `{"google_books": "AIza..."}`).
    pub api_keys: std::collections::HashMap<String, String>,
}

/// Pure conversion from raw `enabled_modules` strings to the toggle map.
/// Extracted for unit-testability (no DB needed).
fn modules_to_fallback_preferences(modules: &[String]) -> std::collections::HashMap<String, bool> {
    let mut prefs = std::collections::HashMap::new();
    for module in modules {
        if module == "enable_google_books" {
            prefs.insert("google_books".to_string(), true);
        } else if let Some(provider) = module.strip_prefix("disable_fallback:") {
            prefs.insert(provider.to_string(), false);
        }
    }
    prefs
}

/// Pure inverse of [`modules_to_fallback_preferences`]: fold a toggle map back
/// into the raw `enabled_modules` list, preserving any unrelated modules (games,
/// etc.) already present. Mirrors `api/profile.rs::update_profile` exactly so the
/// FFI write path and the HTTP write path produce identical `enabled_modules`.
fn apply_fallback_preferences_to_modules(
    mut modules: Vec<String>,
    prefs: &std::collections::HashMap<String, bool>,
) -> Vec<String> {
    for (provider, enabled) in prefs {
        if provider == "google_books" {
            // Opt-in source: presence of the flag means enabled.
            let enable_flag = "enable_google_books".to_string();
            if *enabled {
                if !modules.contains(&enable_flag) {
                    modules.push(enable_flag);
                }
            } else {
                modules.retain(|m| m != &enable_flag);
            }
        } else {
            // Opt-out sources: presence of the disable flag means disabled.
            let disable_flag = format!("disable_fallback:{}", provider);
            if *enabled {
                modules.retain(|m| m != &disable_flag);
            } else if !modules.contains(&disable_flag) {
                modules.push(disable_flag);
            }
        }
    }
    modules
}

/// Pure merge of API-key updates into the existing key map, mirroring
/// `api/profile.rs::update_profile`: an empty value removes the key, a non-empty
/// value inserts/overwrites it, and unrelated keys are preserved. Kept pure (no
/// DB, no logging) so secrets never reach a log sink and the logic stays unit-testable.
fn merge_api_keys(
    mut existing: std::collections::HashMap<String, String>,
    updates: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    for (k, v) in updates {
        if v.is_empty() {
            existing.remove(k);
        } else {
            existing.insert(k.clone(), v.clone());
        }
    }
    existing
}

/// Read the installation profile's search settings (toggles + API keys).
///
/// Used by the Flutter settings screen to display the persisted state of
/// the "Search Sources" section. Returns an error if the profile row is
/// missing; callers should fall back to defaults in that case.
pub async fn installation_profile_get_search_settings() -> Result<FrbSearchSettings, String> {
    use crate::models::installation_profile::ProfileConfig;

    let db = db().ok_or("Database not initialized")?;
    // Reuse the canonical loader so any future schema change stays in one place.
    let profile = ProfileConfig::load(db).await?;

    Ok(FrbSearchSettings {
        fallback_preferences: modules_to_fallback_preferences(&profile.enabled_modules),
        api_keys: profile.api_keys,
    })
}

/// Persist the installation profile's search settings (source toggles + API
/// keys) directly to the database.
///
/// This is the FFI-mode counterpart of `PUT /api/profile`: native clients call
/// it so the persisted state survives an app restart even when the embedded
/// HTTP server is not running. Previously the FFI write path depended on that
/// server and silently no-op'd when it was down, so toggles reverted on reload.
/// Read the value back via [`installation_profile_get_search_settings`].
///
/// Security: `api_keys` holds secrets; they are merged and stored but never
/// logged. The profile row (id 1) is the single source of truth, shared with
/// the HTTP write path, so both stay consistent.
pub async fn installation_profile_set_search_settings(
    fallback_preferences: std::collections::HashMap<String, bool>,
    api_keys: std::collections::HashMap<String, String>,
) -> Result<(), String> {
    use crate::models::installation_profile::{ActiveModel, Entity as ProfileEntity};
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    let db = db().ok_or("Database not initialized")?;

    // Single source of truth: the same profile row (id 1) the HTTP path writes.
    let existing = ProfileEntity::find_by_id(1)
        .one(db)
        .await
        .map_err(|e| format!("Failed to load profile: {e}"))?
        .ok_or("Installation profile not found")?;

    let base_modules: Vec<String> =
        serde_json::from_str(&existing.enabled_modules).unwrap_or_default();
    let modules = apply_fallback_preferences_to_modules(base_modules, &fallback_preferences);

    let existing_keys: std::collections::HashMap<String, String> = existing
        .api_keys
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let merged_keys = merge_api_keys(existing_keys, &api_keys);

    let mut active: ActiveModel = existing.into();
    active.enabled_modules = Set(serde_json::to_string(&modules).map_err(|e| e.to_string())?);
    active.api_keys = Set(if merged_keys.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&merged_keys).map_err(|e| e.to_string())?)
    });
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    active
        .update(db)
        .await
        .map_err(|e| format!("Failed to persist search settings: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod search_settings_conversion_tests {
    use super::{
        apply_fallback_preferences_to_modules, merge_api_keys, modules_to_fallback_preferences,
    };
    use std::collections::HashMap;

    #[test]
    fn empty_modules_yields_empty_prefs() {
        let prefs = modules_to_fallback_preferences(&[]);
        assert!(prefs.is_empty());
    }

    #[test]
    fn enable_google_books_maps_to_true() {
        let prefs = modules_to_fallback_preferences(&["enable_google_books".to_string()]);
        assert_eq!(prefs.get("google_books"), Some(&true));
        assert_eq!(prefs.len(), 1);
    }

    #[test]
    fn disable_fallback_maps_to_false() {
        let prefs = modules_to_fallback_preferences(&[
            "disable_fallback:openlibrary".to_string(),
            "disable_fallback:bnf".to_string(),
        ]);
        assert_eq!(prefs.get("openlibrary"), Some(&false));
        assert_eq!(prefs.get("bnf"), Some(&false));
        assert_eq!(prefs.len(), 2);
    }

    #[test]
    fn unrelated_modules_are_ignored() {
        let prefs = modules_to_fallback_preferences(&[
            "memory_game".to_string(),
            "sliding_puzzle".to_string(),
            "enable_google_books".to_string(),
        ]);
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs.get("google_books"), Some(&true));
    }

    #[test]
    fn round_trip_with_update_profile_format() {
        // Mirror the exact shape that api/profile.rs::update_profile writes
        // when the client sends fallback_preferences:
        //   { google_books: true, inventaire: false, openlibrary: true }
        let modules = vec![
            "enable_google_books".to_string(),
            "disable_fallback:inventaire".to_string(),
            // "openlibrary: true" ⇒ no disable flag, so absent from modules.
        ];
        let prefs = modules_to_fallback_preferences(&modules);
        assert_eq!(prefs.get("google_books"), Some(&true));
        assert_eq!(prefs.get("inventaire"), Some(&false));
        assert!(
            !prefs.contains_key("openlibrary"),
            "providers with neither flag must stay absent so the client default applies"
        );
    }

    #[test]
    fn enabling_google_books_adds_flag_idempotently() {
        let prefs = HashMap::from([("google_books".to_string(), true)]);
        let modules = apply_fallback_preferences_to_modules(vec![], &prefs);
        assert_eq!(modules, vec!["enable_google_books".to_string()]);
        // Toggling on twice must not duplicate the flag.
        let again = apply_fallback_preferences_to_modules(modules, &prefs);
        assert_eq!(again, vec!["enable_google_books".to_string()]);
    }

    #[test]
    fn disabling_google_books_removes_flag_preserving_unrelated() {
        let prefs = HashMap::from([("google_books".to_string(), false)]);
        let modules = apply_fallback_preferences_to_modules(
            vec!["enable_google_books".to_string(), "memory_game".to_string()],
            &prefs,
        );
        assert_eq!(modules, vec!["memory_game".to_string()]);
    }

    #[test]
    fn disabling_optout_provider_adds_disable_flag() {
        let prefs = HashMap::from([("openlibrary".to_string(), false)]);
        let modules =
            apply_fallback_preferences_to_modules(vec!["memory_game".to_string()], &prefs);
        assert!(modules.contains(&"memory_game".to_string()));
        assert!(modules.contains(&"disable_fallback:openlibrary".to_string()));
        // Re-enabling drops the disable flag again.
        let re_enabled = apply_fallback_preferences_to_modules(
            modules,
            &HashMap::from([("openlibrary".to_string(), true)]),
        );
        assert!(!re_enabled.contains(&"disable_fallback:openlibrary".to_string()));
    }

    #[test]
    fn set_then_get_round_trips_the_toggle_intent() {
        // The write conversion must compose with the read conversion back into
        // the original toggle map — this is the invariant the persistence fix
        // relies on (write via FFI, read via FFI).
        let prefs = HashMap::from([
            ("google_books".to_string(), true),
            ("inventaire".to_string(), false),
        ]);
        let modules = apply_fallback_preferences_to_modules(vec![], &prefs);
        let back = modules_to_fallback_preferences(&modules);
        assert_eq!(back.get("google_books"), Some(&true));
        assert_eq!(back.get("inventaire"), Some(&false));
    }

    #[test]
    fn merge_api_keys_inserts_updates_and_removes_empties() {
        let existing = HashMap::from([
            ("google_books".to_string(), "OLD".to_string()),
            ("other".to_string(), "keep".to_string()),
        ]);
        let updates = HashMap::from([
            ("google_books".to_string(), "NEW".to_string()),
            ("removeme".to_string(), String::new()),
        ]);
        let merged = merge_api_keys(existing, &updates);
        assert_eq!(merged.get("google_books"), Some(&"NEW".to_string()));
        assert_eq!(
            merged.get("other"),
            Some(&"keep".to_string()),
            "unrelated keys preserved"
        );
        assert!(
            !merged.contains_key("removeme"),
            "empty value removes the key"
        );
    }
}
