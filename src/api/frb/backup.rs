// Local backup and restore (ADR-037).
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Local Backup (.bgbackup) — FFI (ADR-037) ============

/// Summary of a successfully written `.bgbackup` archive, returned to the
/// Flutter caller. Counts and sizes are surfaced as `i64` for FFI
/// portability (Dart `int` is signed 64-bit on native).
#[frb(dart_metadata = ("freezed"))]
pub struct FrbBackupSummary {
    pub archive_path: String,
    pub archive_size_bytes: i64,
    pub identity_included: bool,
    pub exported_at: String,
    pub library_uuid: String,
    pub schema_version: i64,
    pub format_version: String,
    pub books_count: i64,
    pub copies_count: i64,
    pub loans_count: i64,
    pub contacts_count: i64,
    pub authors_count: i64,
    pub tags_count: i64,
    pub collections_count: i64,
    pub peers_count: i64,
    pub sales_count: i64,
    pub covers_count: i64,
    /// Full manifest serialized as JSON. Already public in clear inside
    /// the archive itself; safe to surface to the UI for diagnostics.
    pub manifest_json: String,
}

impl FrbBackupSummary {
    fn try_from_summary(s: crate::api::backup::BackupSummary) -> Result<Self, String> {
        let manifest_json =
            serde_json::to_string(&s.manifest).map_err(|e| format!("manifest serialize: {e}"))?;
        Ok(Self {
            archive_path: s.archive_path.to_string_lossy().into_owned(),
            archive_size_bytes: s.archive_size_bytes as i64,
            identity_included: s.manifest.identity_included,
            exported_at: s.manifest.exported_at.clone(),
            library_uuid: s.manifest.library_uuid.clone(),
            schema_version: s.manifest.schema_version as i64,
            format_version: s.manifest.format_version.clone(),
            books_count: s.manifest.counts.books as i64,
            copies_count: s.manifest.counts.copies as i64,
            loans_count: s.manifest.counts.loans as i64,
            contacts_count: s.manifest.counts.contacts as i64,
            authors_count: s.manifest.counts.authors as i64,
            tags_count: s.manifest.counts.tags as i64,
            collections_count: s.manifest.counts.collections as i64,
            peers_count: s.manifest.counts.peers as i64,
            sales_count: s.manifest.counts.sales as i64,
            covers_count: s.manifest.covers.len() as i64,
            manifest_json,
        })
    }
}

/// Write a `.bgbackup` archive at `output_path` (ADR-037).
///
/// `unlock_kind` accepts `"recovery_code"` or `"passphrase"`. Any other
/// value yields an error before any heavy work runs.
///
/// `secret_bytes` carries the resolved passphrase or recovery code as raw
/// UTF-8 bytes. Crossing the FFI as `Uint8List` (rather than `String`)
/// lets the Flutter caller clear the buffer in place via
/// `fillRange(0, len, 0)` after the call returns; Dart `String` is
/// immutable and cannot be wiped reliably.
///
/// The Rust side wraps `secret_bytes` in `Zeroizing` so it is also
/// scrubbed on every return path (including panics) before write_backup
/// makes its own internal copy.
///
/// `include_identity = true` packs the Ed25519 + X25519 secret bytes in
/// `identity.bin` (Option C clone mode). The identity must already be
/// initialized via `init_identity_ffi`; otherwise the call returns an
/// error before any file is touched on disk.
///
/// `cover_dir` is the on-disk directory where local cover images live;
/// hub-hosted URLs are detected by their `http(s)://` prefix and skipped.
pub async fn write_backup_ffi(
    output_path: String,
    secret_bytes: Vec<u8>,
    unlock_kind: String,
    library_uuid: String,
    include_identity: bool,
    prefs_json: String,
    cover_dir: String,
) -> Result<FrbBackupSummary, String> {
    use std::path::Path;
    use zeroize::Zeroizing;

    let db_conn = db().ok_or("Database not initialized")?;

    let unlock = match unlock_kind.as_str() {
        "recovery_code" => crate::api::backup::UnlockKind::RecoveryCode,
        "passphrase" => crate::api::backup::UnlockKind::Passphrase,
        other => return Err(format!("invalid unlock_kind: {other}")),
    };

    // Resolve identity bytes BEFORE running heavy work so a misconfigured
    // call fails fast (no Argon2, no VACUUM, no partial archive).
    let identity_bytes = if include_identity {
        let svc = IDENTITY_SERVICE
            .get()
            .ok_or("Identity not initialized; call init_identity_ffi first")?;
        let identity = svc.identity().map_err(|e| e.to_string())?;
        Some(identity.export_secret_bytes())
    } else {
        None
    };

    let secret_owned: Zeroizing<Vec<u8>> = Zeroizing::new(secret_bytes);

    let summary = crate::api::backup::write_backup(
        db_conn,
        Path::new(&output_path),
        &secret_owned,
        unlock,
        &library_uuid,
        identity_bytes,
        &prefs_json,
        Path::new(&cover_dir),
    )
    .await
    .map_err(|e| e.to_string())?;

    FrbBackupSummary::try_from_summary(summary)
}

// ============ Local Backup Restore (.bgbackup) — FFI (ADR-037 §5) ============

/// Subset of the manifest surfaced to the wizard's preview screen. Mirrors
/// `ManifestSummary` field-by-field but flattened for FFI portability.
/// Counts cross the FFI as `i64`.
#[frb(dart_metadata = ("freezed"))]
pub struct FrbBackupManifestPreview {
    pub format_version: String,
    pub schema_version: i64,
    pub current_schema_version: i64,
    pub exported_at: String,
    pub library_uuid: String,
    pub identity_included: bool,
    /// `"recovery_code"` or `"passphrase"` -- drives the wording of the
    /// secret prompt in the wizard.
    pub unlock_kind: String,
    pub app_version: String,
    pub books_count: i64,
    pub copies_count: i64,
    pub loans_count: i64,
    pub contacts_count: i64,
    pub authors_count: i64,
    pub tags_count: i64,
    pub collections_count: i64,
    pub peers_count: i64,
    pub sales_count: i64,
    pub covers_count: i64,
    pub db_sha256: String,
}

impl FrbBackupManifestPreview {
    fn from_manifest(m: crate::api::backup::ManifestSummary) -> Self {
        let unlock_kind = match m.unlock_kind {
            crate::api::backup::UnlockKind::RecoveryCode => "recovery_code".to_string(),
            crate::api::backup::UnlockKind::Passphrase => "passphrase".to_string(),
        };
        Self {
            format_version: m.format_version,
            schema_version: m.schema_version as i64,
            current_schema_version: crate::infrastructure::db::SCHEMA_VERSION as i64,
            exported_at: m.exported_at,
            library_uuid: m.library_uuid,
            identity_included: m.identity_included,
            unlock_kind,
            app_version: m.app_version,
            books_count: m.counts.books as i64,
            copies_count: m.counts.copies as i64,
            loans_count: m.counts.loans as i64,
            contacts_count: m.counts.contacts as i64,
            authors_count: m.counts.authors as i64,
            tags_count: m.counts.tags as i64,
            collections_count: m.counts.collections as i64,
            peers_count: m.counts.peers as i64,
            sales_count: m.counts.sales as i64,
            covers_count: m.covers.len() as i64,
            db_sha256: m.db_sha256,
        }
    }
}

/// Outcome of a successful restore returned to the wizard.
#[frb(dart_metadata = ("freezed"))]
pub struct FrbRestoreSummary {
    /// `"replace"` or `"merge"`.
    pub mode: String,
    pub identity_restored: bool,
    /// True iff the caller passed a `local_library_uuid` matching the
    /// archive's manifest UUID -- the typical auto-backup restore on the
    /// device that produced the archive. The Replace path then preserves
    /// `crypto_keys` and the Flutter caller leaves local storage alone
    /// (ADR-037 §5).
    pub same_device: bool,
    /// Set on Replace + identity restored: caller MUST persist this UUID to
    /// both Keychain and SharedPreferences (dual-write hardening per
    /// `e2ee_identity_storage_fragility.md`). On Replace without identity
    /// AND cross-device, caller MUST clear the UUID from both stores. On
    /// Merge or Replace same-device, caller MUST NOT touch the existing
    /// UUID (`null`).
    pub restored_library_uuid: Option<String>,
    /// `"clear"` (Replace + cross-device + no identity), `"set"`
    /// (Replace + identity), `"keep"` (Merge or Replace same-device).
    /// Drives the Flutter post-restore action.
    pub library_uuid_action: String,
    pub prefs_json: String,
    pub rollback_path: Option<String>,
    pub books_after: i64,
    pub copies_after: i64,
    pub contacts_after: i64,
    pub covers_restored: i64,
}

impl FrbRestoreSummary {
    fn from_summary(s: crate::api::backup::RestoreSummary) -> Self {
        let mode = match s.mode {
            crate::api::backup::RestoreMode::Replace => "replace",
            crate::api::backup::RestoreMode::Merge => "merge",
        }
        .to_string();
        // Replace + same_device collapses to "keep": the local
        // `library_uuid` already matches the manifest's, so the caller
        // must not touch its own storage. Replace + clone-mode still
        // returns "set" (caller writes the manifest UUID locally), and
        // Replace + cross-device + no clone returns "clear" (caller
        // wipes its UUID so the next launch generates a fresh one).
        let library_uuid_action = match (s.mode, s.identity_restored, s.same_device) {
            (crate::api::backup::RestoreMode::Merge, _, _) => "keep",
            (crate::api::backup::RestoreMode::Replace, true, _) => "set",
            (crate::api::backup::RestoreMode::Replace, false, true) => "keep",
            (crate::api::backup::RestoreMode::Replace, false, false) => "clear",
        }
        .to_string();
        Self {
            mode,
            identity_restored: s.identity_restored,
            same_device: s.same_device,
            restored_library_uuid: s.restored_library_uuid,
            library_uuid_action,
            prefs_json: s.prefs_json,
            rollback_path: s.rollback_path,
            books_after: s.books_after,
            copies_after: s.copies_after,
            contacts_after: s.contacts_after,
            covers_restored: s.covers_restored,
        }
    }
}

/// Available rollback file presented in the "Restore previous version" UI.
#[frb(dart_metadata = ("freezed"))]
pub struct FrbRollbackInfo {
    pub path: String,
    pub created_at: String,
    pub age_seconds: i64,
    pub size_bytes: i64,
}

impl FrbRollbackInfo {
    fn from_info(i: crate::api::backup::RollbackInfo) -> Self {
        Self {
            path: i.path,
            created_at: i.created_at,
            age_seconds: i.age_seconds,
            size_bytes: i.size_bytes,
        }
    }
}

/// Parse `manifest.json` from a `.bgbackup` file without unlocking. Used by
/// the wizard preview step.
pub async fn read_manifest_ffi(archive_path: String) -> Result<FrbBackupManifestPreview, String> {
    use std::path::Path;
    // Stays on the spawn_blocking pool: parsing the zip + manifest is sync IO.
    let archive_path = archive_path.clone();
    let manifest = tokio::task::spawn_blocking(move || {
        crate::api::backup::read_manifest(Path::new(&archive_path))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(FrbBackupManifestPreview::from_manifest(manifest))
}

/// Restore a `.bgbackup` archive into the live DB.
///
/// `mode` accepts `"replace"` or `"merge"`. `restore_identity` is honoured
/// only in Replace mode AND when the archive carries `identity.bin`; ignored
/// otherwise.
///
/// **The Flutter caller MUST force-restart the app after this returns.** The
/// FFI's global SeaORM connection still holds open file descriptors on the
/// inode that was just renamed to the rollback sibling; reusing it is unsafe.
/// `db_path` and `cover_dir` are passed explicitly so the caller (which knows
/// its sandbox layout via `getApplicationSupportDirectory`) is the single
/// source of truth.
/// `local_library_uuid`: pass the device's current `library_uuid` so the
/// Replace path can detect a same-device restore and keep `crypto_keys`
/// intact (ADR-037 §5). `None` falls back to the cross-device behaviour
/// (wipe identity unless full clone-mode).
pub async fn restore_backup_ffi(
    archive_path: String,
    secret_bytes: Vec<u8>,
    mode: String,
    restore_identity: bool,
    local_library_uuid: Option<String>,
    db_path: String,
    cover_dir: String,
) -> Result<FrbRestoreSummary, String> {
    use std::path::Path;
    use zeroize::Zeroizing;

    let restore_mode = match mode.as_str() {
        "replace" => crate::api::backup::RestoreMode::Replace,
        "merge" => crate::api::backup::RestoreMode::Merge,
        other => return Err(format!("invalid mode: {other}")),
    };

    let secret_owned: Zeroizing<Vec<u8>> = Zeroizing::new(secret_bytes);

    let summary = crate::api::backup::restore_backup(
        Path::new(&archive_path),
        &secret_owned,
        restore_mode,
        restore_identity,
        local_library_uuid,
        Path::new(&db_path),
        Path::new(&cover_dir),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(FrbRestoreSummary::from_summary(summary))
}

/// List rollback files available in the directory of `db_path`. Empty list
/// means no "Restore previous version" card should be shown.
pub async fn list_available_rollbacks_ffi(db_path: String) -> Result<Vec<FrbRollbackInfo>, String> {
    use std::path::Path;
    let path = db_path.clone();
    let infos = tokio::task::spawn_blocking(move || {
        crate::api::backup::list_available_rollbacks(Path::new(&path))
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(infos.into_iter().map(FrbRollbackInfo::from_info).collect())
}

/// Swap a rollback file back into the live DB. Same restart constraint as
/// `restore_backup_ffi`.
pub async fn restore_from_rollback_ffi(
    rollback_path: String,
    db_path: String,
) -> Result<(), String> {
    use std::path::Path;
    crate::api::backup::restore_from_rollback(Path::new(&rollback_path), Path::new(&db_path))
        .await
        .map_err(|e| e.to_string())
}

/// Watermark used by the Flutter `BackupSchedulerService` to skip a 24h tick
/// when no catalog change happened. Returns the highest `updated_at` across
/// `books`, `copies`, `loans`, `library_config`, or `None` for a fresh DB
/// (ADR-037 §6).
pub async fn latest_user_data_change_at_ffi() -> Result<Option<String>, String> {
    let db_conn = db().ok_or("Database not initialized")?;
    crate::api::backup::latest_user_data_change_at(db_conn)
        .await
        .map_err(|e| e.to_string())
}
