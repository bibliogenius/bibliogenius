// E2EE account sync: enrollment, device pairing, sync runs, logout, backend shutdown.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Account E2EE Sync — session + enrollment ============
//
// Exposes the already-built account-sync services (enrollment, signed device registry,
// at-rest persistence) to Flutter. Scope here: JOIN an existing account (passphrase or
// sealed QR pairing), manage authorized devices, and hold the unlocked trousseau in RAM
// across calls. Out of scope (separate slices): account SIGNUP (needs the descriptor
// signer + recovery kit + passphrase policy) and the data sync cycle `sync_once` (needs
// the data merge engine, not built yet). `account_refresh_devices_ffi` already drives the real
// registry adoption that `sync_once` will sit behind.

/// The unlocked account session, held in RAM for the running process. Dropping it zeroizes
/// the trousseau (A1). Rehydrated from the encrypted `account_session` row on first use
/// after launch via [`ensure_account_session`].
struct AccountSession {
    /// Shared with the client so it can re-login on its own when the hub expires the
    /// session token (`enable_auto_reauth`), without a second copy of the trousseau in RAM.
    bundle: std::sync::Arc<crate::crypto::account_keys::AccountKeyBundle>,
    client: crate::services::account_sync_client::AccountSyncClient,
    account_id: String,
    device_id: String,
    email: String,
}

static ACCOUNT_SESSION: tokio::sync::Mutex<Option<AccountSession>> =
    tokio::sync::Mutex::const_new(None);

/// The random `device_id` lane key a NEW device generates when it shows its pairing QR,
/// kept until the sealed trousseau comes back so it enrolls under the SAME id the
/// authorizing device wrote into the registry. RAM-only: pairing is one in-person flow.
static PENDING_PAIRING_DEVICE_ID: tokio::sync::Mutex<Option<String>> =
    tokio::sync::Mutex::const_new(None);

/// The library UUID this device's identity was initialized with (the at-rest KDF input).
fn account_library_uuid() -> Result<String, String> {
    IDENTITY_SERVICE
        .get()
        .ok_or("Identity not initialized")?
        .library_uuid()
        .map(|s| s.to_string())
        .ok_or_else(|| "Library UUID not available".to_string())
}

/// This device's `DeviceEntry` for the signed registry: its random lane key plus its
/// reused ADR-039 `NodeIdentity` public keys.
fn account_device_entry(
    device_id: &str,
    name: &str,
) -> Result<crate::crypto::device_registry::DeviceEntry, String> {
    let svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;
    let identity = svc.identity()?;
    Ok(crate::crypto::device_registry::DeviceEntry {
        device_id: device_id.to_string(),
        ed25519_pk: identity.verifying_key().to_bytes(),
        x25519_pk: identity.x25519_public_key().to_bytes(),
        name: name.to_string(),
    })
}

/// Whether enrollment must be followed by an app restart to activate data sync.
///
/// CRR-ification (and the cr-sqlite pool) is gated on enrollment + a restart: a device
/// that just enrolled booted in plain mode, so its replicated tables are not yet CRRs and
/// the merge engine cannot run until the next boot flips it into sync mode (see
/// `db::init_db_account_sync`). True iff the live database has no CRRs yet; on builds
/// without account sync there is no cr-sqlite, so a restart changes nothing → false.
#[cfg(feature = "account_sync")]
async fn enrollment_restart_required(db: &DatabaseConnection) -> bool {
    // On error, assume a restart is needed (safer: the user reboots and `setup_crrs` runs).
    !crate::infrastructure::crsqlite_crr::crrs_present(db)
        .await
        .unwrap_or(false)
}
#[cfg(not(feature = "account_sync"))]
async fn enrollment_restart_required(_db: &DatabaseConnection) -> bool {
    false
}

/// Install the freshly unlocked session into the process-global slot, replacing any prior
/// one (dropping it zeroizes the old trousseau, A1). Call AFTER building the status JSON,
/// since this takes ownership of the metadata fields.
async fn store_account_session(
    bundle: crate::crypto::account_keys::AccountKeyBundle,
    mut client: crate::services::account_sync_client::AccountSyncClient,
    account_id: String,
    device_id: String,
    email: String,
) {
    let bundle = std::sync::Arc::new(bundle);
    // The hub expires the session token 30 minutes after login and never slides the TTL,
    // while this process can stay up for days: let the client mint itself a new one.
    client.enable_auto_reauth(email.clone(), std::sync::Arc::clone(&bundle));
    *ACCOUNT_SESSION.lock().await = Some(AccountSession {
        bundle,
        client,
        account_id,
        device_id,
        email,
    });
}

/// JSON status object for the UI: signed-in flag plus the plaintext metadata.
fn account_status_json(signed_in: bool, email: &str, account_id: &str, device_id: &str) -> String {
    serde_json::json!({
        "signed_in": signed_in,
        "email": email,
        "account_id": account_id,
        "device_id": device_id,
    })
    .to_string()
}

/// Status JSON returned by the join / sealed-pairing enrollment FFIs: the signed-in
/// metadata plus the restart-required flag. (Signup builds its own variant since it also
/// carries the one-time recovery phrase.)
fn enrollment_status_json(
    email: &str,
    account_id: &str,
    device_id: &str,
    restart_required: bool,
) -> String {
    serde_json::json!({
        "signed_in": true,
        "email": email,
        "account_id": account_id,
        "device_id": device_id,
        "restart_required": restart_required,
    })
    .to_string()
}

/// Rehydrate the in-RAM session from disk if empty: decrypt the trousseau under the
/// device-local key and re-authenticate with the hub. Errors if no session is persisted.
async fn ensure_account_session<'a>(
    guard: &'a mut tokio::sync::MutexGuard<'_, Option<AccountSession>>,
) -> Result<&'a mut AccountSession, String> {
    if guard.is_none() {
        let db = db().ok_or("Database not initialized")?;
        let lib = account_library_uuid()?;
        let persisted = crate::services::account_session_service::load(db, &lib)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("No account session on this device")?;
        let mut client = crate::services::account_sync_client::AccountSyncClient::new()
            .map_err(|e| e.to_string())?;
        // Re-authenticate from the trousseau (the stored token is not persisted).
        client
            .login(&persisted.email, &persisted.bundle)
            .await
            .map_err(|e| e.to_string())?;
        let bundle = std::sync::Arc::new(persisted.bundle);
        // The token minted just above dies after 30 minutes (the hub's session TTL) and
        // this session object outlives it: arm the client's own renewal, or every call
        // past that mark 401s until the app restarts.
        client.enable_auto_reauth(persisted.email.clone(), std::sync::Arc::clone(&bundle));
        **guard = Some(AccountSession {
            bundle,
            client,
            account_id: persisted.account_id,
            device_id: persisted.device_id,
            email: persisted.email,
        });
    }
    Ok(guard.as_mut().expect("session populated above"))
}

/// Account session status for the UI. Cheap: reads the plaintext metadata columns and
/// never decrypts the trousseau or hits the network.
pub async fn account_status_ffi() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::account_session_service::load_metadata(db)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(m) => Ok(account_status_json(
            true,
            &m.email,
            &m.account_id,
            &m.device_id,
        )),
        None => Ok(account_status_json(false, "", "", "")),
    }
}

/// Join an EXISTING account on this device with its passphrase (Path A). Unlocks the
/// trousseau, enrolls this device into the signed registry, and persists the session.
pub async fn account_enroll_passphrase_ffi(
    email: String,
    passphrase: String,
    device_name: String,
) -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    let lib = account_library_uuid()?;

    let mut client = crate::services::account_sync_client::AccountSyncClient::new()
        .map_err(|e| e.to_string())?;
    let pass = secrecy::SecretString::new(passphrase);
    let enrolled =
        crate::services::account_enrollment::enroll_with_passphrase(&mut client, &email, &pass)
            .await
            .map_err(|e| e.to_string())?;

    // Add this device to the account's signed registry so peers accept its lanes (H3).
    let device_id = crate::services::account_session_service::generate_device_id();
    let entry = account_device_entry(&device_id, &device_name)?;
    let state = crate::services::account_sync_engine::DbSyncStateStore::new(db.clone());
    crate::services::account_sync_engine::enroll_device(
        &client,
        &state,
        &enrolled.bundle,
        &enrolled.account_id,
        entry,
    )
    .await
    .map_err(|e| e.to_string())?;

    crate::services::account_session_service::persist(
        db,
        &lib,
        &enrolled.account_id,
        &email,
        &device_id,
        &enrolled.bundle,
    )
    .await
    .map_err(|e| e.to_string())?;

    let restart_required = enrollment_restart_required(db).await;
    let status = enrollment_status_json(&email, &enrolled.account_id, &device_id, restart_required);
    store_account_session(
        enrolled.bundle,
        client,
        enrolled.account_id,
        device_id,
        email,
    )
    .await;
    Ok(status)
}

/// NEW device, step 1: generate this device's lane key and return the `bg-pair` QR payload
/// (its lane key + ADR-039 public keys) for an authorized device to scan. The payload
/// carries NO secret; the X25519 public key in it is what the authorized device will seal
/// the trousseau to (ADR-045 authenticated channel).
pub async fn account_get_device_pairing_qr_ffi(device_name: String) -> Result<String, String> {
    let svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;
    let identity = svc.identity()?;
    let device_id = crate::services::account_session_service::generate_device_id();
    let payload = crate::services::account_pairing::build_pairing_qr(
        &device_id,
        &identity.verifying_key().to_bytes(),
        &identity.x25519_public_key().to_bytes(),
        &device_name,
    );
    *PENDING_PAIRING_DEVICE_ID.lock().await = Some(device_id);
    Ok(payload)
}

/// AUTHORIZED device: scan a NEW device's `bg-pair` QR, seal the trousseau to it, and add it
/// to the signed registry. Returns the `bg-sealed` payload (sealed blob + account email) to
/// show back as a QR.
///
/// SECURITY (ADR-045 / ADR-042 §14 H2): the X25519 key the trousseau is sealed to comes
/// ONLY from the scanned payload (`req.x25519_pk`) — never a hub field. Do not refactor this
/// to source the key from anywhere else.
pub async fn account_authorize_device_ffi(pairing_qr_payload: String) -> Result<String, String> {
    let req = crate::services::account_pairing::parse_pairing_qr(&pairing_qr_payload)
        .map_err(|e| e.to_string())?;
    let db = db().ok_or("Database not initialized")?;

    let mut guard = ACCOUNT_SESSION.lock().await;
    let session = ensure_account_session(&mut guard).await?;

    // Seal to the scanned key, and only the scanned key.
    let sealed = session
        .bundle
        .seal_to_device(&req.x25519_pk)
        .map_err(|e| e.to_string())?;

    let state = crate::services::account_sync_engine::DbSyncStateStore::new(db.clone());
    crate::services::account_sync_engine::enroll_device(
        &session.client,
        &state,
        &session.bundle,
        &session.account_id,
        req.to_device_entry(),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(crate::services::account_pairing::build_sealed_qr(
        &sealed,
        &session.email,
    ))
}

/// NEW device, step 2: scan the `bg-sealed` QR returned by the authorized device, open the
/// trousseau with this device's X25519 identity, authenticate, and persist the session. The
/// authorizing device already registered this device, so no registry write happens here.
pub async fn account_enroll_from_sealed_ffi(sealed_qr_payload: String) -> Result<String, String> {
    let (sealed, email) = crate::services::account_pairing::parse_sealed_qr(&sealed_qr_payload)
        .map_err(|e| e.to_string())?;
    let db = db().ok_or("Database not initialized")?;
    let lib = account_library_uuid()?;
    let device_id = PENDING_PAIRING_DEVICE_ID
        .lock()
        .await
        .clone()
        .ok_or("No pending pairing on this device; show the pairing QR first")?;

    let svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;
    let identity = svc.identity()?;
    let mut client = crate::services::account_sync_client::AccountSyncClient::new()
        .map_err(|e| e.to_string())?;
    let enrolled = crate::services::account_enrollment::enroll_from_sealed_bundle(
        &mut client,
        &email,
        identity,
        &sealed,
    )
    .await
    .map_err(|e| e.to_string())?;

    crate::services::account_session_service::persist(
        db,
        &lib,
        &enrolled.account_id,
        &email,
        &device_id,
        &enrolled.bundle,
    )
    .await
    .map_err(|e| e.to_string())?;

    *PENDING_PAIRING_DEVICE_ID.lock().await = None;
    let restart_required = enrollment_restart_required(db).await;
    let status = enrollment_status_json(&email, &enrolled.account_id, &device_id, restart_required);
    store_account_session(
        enrolled.bundle,
        client,
        enrolled.account_id,
        device_id,
        email,
    )
    .await;
    Ok(status)
}

/// Fetch and adopt the account's signed device registry, returning the authorized devices
/// as JSON (`{device_id, name, is_self}`). This is the H3 step `sync_once` runs first; it
/// is exposed on its own so the UI can list/refresh devices before data sync ships.
pub async fn account_refresh_devices_ffi() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    let mut guard = ACCOUNT_SESSION.lock().await;
    let session = ensure_account_session(&mut guard).await?;

    let state = crate::services::account_sync_engine::DbSyncStateStore::new(db.clone());
    let registry = crate::services::account_sync_engine::refresh_authorized_devices(
        &session.client,
        &state,
        &session.account_id,
        &session.bundle.verifying_key(),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(account_devices_json(
        registry
            .as_ref()
            .map(|r| r.devices.as_slice())
            .unwrap_or(&[]),
        &session.device_id,
    ))
}

/// Serialize an authorized-device list for the UI, tagging the current device with
/// `is_self` (the UI hides the "remove" action on it — self-removal goes through
/// `account_logout_ffi` instead). Shared by the refresh and remove FFIs.
fn account_devices_json(
    devices: &[crate::crypto::device_registry::DeviceEntry],
    self_device_id: &str,
) -> String {
    let list: Vec<serde_json::Value> = devices
        .iter()
        .map(|d| {
            serde_json::json!({
                "device_id": d.device_id,
                "name": d.name,
                "is_self": d.device_id == self_device_id,
            })
        })
        .collect();
    serde_json::json!({ "devices": list }).to_string()
}

/// Remove another device from the account's signed registry (soft revocation): shrink
/// the registry, bump its seq, re-sign, and publish, so every other device stops
/// applying the removed device's lanes (H3). Returns the refreshed device list JSON.
///
/// Refuses to remove THIS device (`device_id == session.device_id`): dropping the current
/// device from its own registry would strip its lanes while leaving it signed in — a
/// footgun. Leaving the account on this device is `account_logout_ffi` instead.
///
/// This is a soft, not cryptographic, removal — the removed device keeps the trousseau
/// and can still read current content or re-add itself; a hard lockout needs key rotation
/// (deferred, ADR-042 section 13.5).
pub async fn account_remove_device_ffi(device_id: String) -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    let mut guard = ACCOUNT_SESSION.lock().await;
    let session = ensure_account_session(&mut guard).await?;

    if device_id == session.device_id {
        return Err("cannot remove the current device; use sign out instead".to_string());
    }

    let state = crate::services::account_sync_engine::DbSyncStateStore::new(db.clone());
    let updated = crate::services::account_sync_engine::remove_device(
        &session.client,
        &state,
        &session.bundle,
        &session.account_id,
        &device_id,
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(account_devices_json(&updated.devices, &session.device_id))
}

/// Trigger a sync cycle for this account. The entrypoint the
/// automatic sync triggers and a manual refresh will share.
///
/// On account-sync builds it runs a full cycle through
/// [`account_sync_engine::refresh_then_sync`]: refresh + adopt the signed device
/// registry (H3) FIRST, then pull/apply and push the data lanes through the real
/// cr-sqlite merge engine over the library DB. Returns JSON `{synced, applied, pushed}`.
///
/// On default builds (no cr-sqlite linked) it still runs the real, available step —
/// refreshing the signed registry — and **deliberately does not fake a data
/// convergence**: the data leg is a no-op, reported honestly as
/// `{synced:false, reason:"data_engine_unavailable", devices}`.
/// Whether this build can actually converge data across devices, i.e. it was
/// compiled with the `account_sync` feature (a cr-sqlite merge engine is linked).
/// The Flutter auto-sync scheduler queries this once and stays fully inert on
/// default builds, where [`account_sync_now_ffi`]'s data leg is a no-op: no point
/// waking a periodic timer or hitting the network for a sync that cannot happen.
pub fn account_sync_capable_ffi() -> bool {
    cfg!(feature = "account_sync")
}

/// Log a failed sync cycle on its way out to the caller.
///
/// Auto-sync runs unattended every 15 minutes, so a cycle that only reports to its caller
/// fails invisibly: the log must show the failures, not just the `data sync complete` lines.
fn log_sync_failure(e: impl std::fmt::Display) -> String {
    tracing::warn!(error = %e, "account_sync_now: sync cycle failed");
    e.to_string()
}

pub async fn account_sync_now_ffi() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    let mut guard = ACCOUNT_SESSION.lock().await;
    let session = ensure_account_session(&mut guard)
        .await
        .map_err(log_sync_failure)?;

    let state = crate::services::account_sync_engine::DbSyncStateStore::new(db.clone());

    #[cfg(feature = "account_sync")]
    {
        // CRR-ification is gated on enrollment + a restart. If this device enrolled
        // but has not restarted yet, the replicated tables are not CRRs and the merge
        // engine cannot run: report it honestly so the UI can prompt for a restart
        // rather than failing on a missing `crsql_*` function.
        if !crate::infrastructure::crsqlite_crr::crrs_present(db)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(serde_json::json!({
                "synced": false,
                "reason": "restart_required",
            })
            .to_string());
        }

        // The merge engine shares the app's single cr-sqlite connection (the pool is
        // pinned to one connection on account-sync builds, see `db::init_db_account_sync`).
        let engine = crate::services::crsqlite_engine::CrSqliteMergeEngine::new(db.clone());
        // Custom cover bytes ride their own lanes (ADR-046): cr-sqlite syncs the
        // cover_url row but not the file. The covers directory is registered in
        // `init_backend`; without it (server binary) covers are not transported.
        let stats = if let Some(covers_dir) = covers_dir() {
            let cover_source =
                crate::services::cover_sync::DbCoverSource::new(db.clone(), covers_dir.clone());
            let cover_sink =
                crate::services::cover_sync::FsCoverSink::new(db.clone(), covers_dir.clone());
            let stats = crate::services::account_sync_engine::refresh_then_sync_with_covers(
                &session.client,
                &engine,
                &session.bundle,
                &state,
                &session.account_id,
                &session.device_id,
                &cover_source,
                &cover_sink,
            )
            .await
            .map_err(log_sync_failure)?;
            // Record the covers we pushed in the local dedup state, now that the
            // whole cycle succeeded (ADR-046): the next sync skips them while their
            // file mtime is unchanged, so periodic auto-sync does not re-encode and
            // re-upload every cover each cycle. Best-effort: the data already
            // converged on the hub, so a local bookkeeping write failure must NOT
            // surface the cycle as failed; the worst case is re-pushing those
            // covers next cycle (idempotent).
            if let Err(e) = cover_source.mark_pushed(&stats.pushed_covers).await {
                tracing::warn!(error = %e, "failed to record pushed covers in dedup state; they will re-push next cycle");
            }
            stats
        } else {
            crate::services::account_sync_engine::refresh_then_sync(
                &session.client,
                &engine,
                &session.bundle,
                &state,
                &session.account_id,
                &session.device_id,
            )
            .await
            .map_err(log_sync_failure)?
        };
        tracing::info!(
            applied = stats.applied,
            pushed = stats.pushed,
            "account_sync_now: data sync complete"
        );
        Ok(serde_json::json!({
            "synced": true,
            "applied": stats.applied,
            "pushed": stats.pushed,
        })
        .to_string())
    }

    #[cfg(not(feature = "account_sync"))]
    {
        // Real step: refresh + adopt the signed registry (anti-rollback). This is the
        // H3 prerequisite `refresh_then_sync` runs before any data sync.
        let registry = crate::services::account_sync_engine::refresh_authorized_devices(
            &session.client,
            &state,
            &session.account_id,
            &session.bundle.verifying_key(),
        )
        .await
        .map_err(log_sync_failure)?;
        let device_count = registry.map(|r| r.devices.len()).unwrap_or(0);

        // No cr-sqlite merge engine in this binary: skip the data leg honestly rather
        // than simulate a convergence that did not happen.
        tracing::info!(
            device_count,
            "account_sync_now: refreshed authorized devices; data sync skipped (merge engine unavailable in this build)"
        );
        Ok(serde_json::json!({
            "synced": false,
            "reason": "data_engine_unavailable",
            "devices": device_count,
        })
        .to_string())
    }
}

/// Release the database's cr-sqlite state before the app process exits.
///
/// cr-sqlite requires `crsql_finalize()` on any connection that touched a CRR or it
/// can abort on teardown. Flutter calls this from its app-lifecycle shutdown. On
/// builds without account sync there is no cr-sqlite state, so this is a no-op.
/// Best-effort and idempotent: a failure is logged, never surfaced, because the
/// process is on its way down.
pub async fn shutdown_backend_ffi() -> Result<String, String> {
    #[cfg(feature = "account_sync")]
    if let Some(db) = db()
        && let Err(e) = crate::infrastructure::crsqlite_crr::finalize(db).await
    {
        tracing::warn!("crsql_finalize on shutdown failed: {e}");
        return Ok("finalize failed".to_string());
    }
    Ok("OK".to_string())
}

/// Sign out of the account on this device: drop the in-RAM session and delete the encrypted
/// `account_session` row. Does not revoke the device server-side (that is a registry edit
/// from another device). Idempotent.
pub async fn account_logout_ffi() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;

    // Demote the replicated tables from CRRs back to plain tables so the database is
    // no longer locked to cr-sqlite builds (writable by any build), reversing the
    // enrollment lock-in. Done before clearing the session row so the invariant
    // "CRRs present OR account_session row == sync mode" never leaves CRRs behind
    // without a session to explain them. Best-effort: a failure must not block sign-out.
    #[cfg(feature = "account_sync")]
    if let Err(e) = crate::infrastructure::crsqlite_crr::teardown_crrs(db).await {
        tracing::warn!("crsql teardown on logout failed: {e}");
    }

    crate::services::account_session_service::clear(db)
        .await
        .map_err(|e| e.to_string())?;
    *ACCOUNT_SESSION.lock().await = None;
    *PENDING_PAIRING_DEVICE_ID.lock().await = None;
    Ok("Signed out".to_string())
}

/// Score a candidate passphrase locally for the signup strength meter. No network, no DB,
/// no logging (SECURITY_GUIDELINES F7: the check is 100% local). Returns JSON
/// `{score 0-4, length, acceptable, warning, suggestions}`; the UI gates the signup button
/// on `acceptable` (zxcvbn 4/4 AND length >= 12).
pub async fn account_check_passphrase_ffi(passphrase: String) -> Result<String, String> {
    let s = crate::services::account_signup_service::check_passphrase(&passphrase);
    Ok(serde_json::json!({
        "score": s.score,
        "length": s.length,
        "acceptable": s.acceptable,
        "warning": s.warning,
        "suggestions": s.suggestions,
    })
    .to_string())
}

/// Create a NEW account on this (first) device with a passphrase (Path A). Enforces the
/// strength floor, generates and double-wraps the trousseau, publishes the first signed
/// registry, persists the session, and returns the one-time BIP39 recovery phrase inside the
/// status JSON (`recovery_phrase`) for the UI to display ONCE. The phrase is never persisted
/// or logged; losing both passphrase and kit means permanent account loss (ADR-042 §8).
pub async fn account_signup_ffi(
    email: String,
    passphrase: String,
    device_name: String,
) -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    let lib = account_library_uuid()?;
    let device_id = crate::services::account_session_service::generate_device_id();
    let entry = account_device_entry(&device_id, &device_name)?;

    let mut client = crate::services::account_sync_client::AccountSyncClient::new()
        .map_err(|e| e.to_string())?;
    let pass = secrecy::SecretString::new(passphrase);
    let outcome =
        match crate::services::account_signup_service::signup(&mut client, &email, &pass, entry)
            .await
        {
            Ok(o) => o,
            // Stable, routable prefixes so the Flutter layer can recover instead of dead-ending:
            // a duplicate email should offer "sign in" (the user can join with their passphrase).
            Err(crate::services::account_signup_service::SignupError::AccountExists) => {
                return Err(format!(
                    "{}: an account already exists for this email",
                    crate::services::account_signup_service::E_ACCOUNT_EXISTS
                ));
            }
            Err(crate::services::account_signup_service::SignupError::WeakPassphrase(_)) => {
                return Err(format!(
                    "{}: passphrase does not meet the strength floor",
                    crate::services::account_signup_service::E_WEAK_PASSPHRASE
                ));
            }
            Err(e) => return Err(e.to_string()),
        };

    crate::services::account_session_service::persist(
        db,
        &lib,
        &outcome.account_id,
        &email,
        &device_id,
        &outcome.bundle,
    )
    .await
    .map_err(|e| e.to_string())?;

    let restart_required = enrollment_restart_required(db).await;
    let status = serde_json::json!({
        "signed_in": true,
        "email": email,
        "account_id": outcome.account_id,
        "device_id": device_id,
        "recovery_phrase": outcome.recovery_phrase,
        "restart_required": restart_required,
    })
    .to_string();
    store_account_session(outcome.bundle, client, outcome.account_id, device_id, email).await;
    Ok(status)
}
