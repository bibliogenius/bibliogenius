// Hub follows, borrow requests, sealed contact blobs.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

/// Sends a follow request to a library.
pub async fn hub_directory_follow(node_id: String) -> Result<FrbHubFollow, String> {
    let db = hub_db()?;

    // Send local X25519 public key so the followed library can encrypt contact for us
    let x25519_key: Option<String> = {
        use sea_orm::ConnectionTrait;
        let row = db
            .query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT public_key FROM crypto_keys WHERE key_type = 'x25519' LIMIT 1".to_owned(),
            ))
            .await
            .ok()
            .flatten();
        row.map(|r| {
            let bytes: Vec<u8> =
                sea_orm::TryGetable::try_get(&r, "", "public_key").unwrap_or_default();
            hex::encode(bytes)
        })
        .filter(|s| !s.is_empty())
    };

    hub_directory_svc()
        .follow(db, &node_id, x25519_key.as_deref())
        .await
        .map(FrbHubFollow::from)
        .map_err(|e| e.to_string())
}

/// Lists incoming follow requests pending approval.
pub async fn hub_directory_pending_requests() -> Result<Vec<FrbHubFollow>, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .pending_requests(db)
        .await
        .map(|v| v.into_iter().map(FrbHubFollow::from).collect())
        .map_err(|e| e.to_string())
}

/// Resolves a pending follow request. resolution: "approve" | "reject" | "block"
/// When approving, encrypted_contact is an optional sealed blob of the owner's contact info.
/// follower_node_id and follower_x25519_public_key (hex) let the owner put
/// the follower on its catalog readers list (ADR-072 D4, D8) and, when the
/// library holds a catalog key, hand it its wrap (D3). A paired peer is
/// pinned and wrapped for with the key exchanged at pairing, never with the
/// one the hub returns. owner_gesture is false for the paired-peer
/// auto-approval (ADR-053), which is refused for a library whose pairing
/// the owner ended; the owner's own approval restores that pairing.
pub async fn hub_directory_resolve_follow(
    follow_id: i64,
    resolution: String,
    encrypted_contact: Option<String>,
    follower_x25519_public_key: Option<String>,
    follower_node_id: Option<String>,
    owner_gesture: bool,
) -> Result<FrbHubFollow, String> {
    use crate::services::hub_catalog_readers;

    let db = hub_db()?;
    if resolution == "approve" {
        hub_catalog_readers::admit_approval(db, follower_node_id.as_deref(), owner_gesture).await?;
    }
    let reader = match (resolution.as_str(), follower_node_id.as_deref()) {
        ("approve", Some(node_id)) => {
            match hub_catalog_readers::key_for_approval(
                db,
                node_id,
                follower_x25519_public_key.as_deref(),
            )
            .await
            {
                Ok(found) => found.map(|(key, source)| (node_id, key, source)),
                Err(e) => {
                    tracing::warn!(
                        target: "hub_directory",
                        "follow {follow_id}: no catalog reader key: {e}"
                    );
                    None
                }
            }
        }
        _ => None,
    };
    // Best-effort: approving is the user's gesture and must not depend on
    // the key store. A missing wrap is caught up by the next wrap batch.
    let wrap = match &reader {
        Some((_, key, _)) => match catalog_key_wrap_for(db, key).await {
            Ok(wrap) => wrap,
            Err(e) => {
                tracing::warn!(
                    target: "hub_directory",
                    "follow {follow_id} approved without a catalog key wrap: {e}"
                );
                None
            }
        },
        None => None,
    };
    let follow = hub_directory_svc()
        .resolve_follow(
            db,
            follow_id,
            &resolution,
            encrypted_contact.as_deref(),
            wrap.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())?;
    // Only once the hub accepted the approval, and only for the library it
    // says it approved: the node id comes from a pending list that may be
    // stale. Left off the list, the follower counts as unknown and the
    // catalog stays in clear.
    if let Some((node_id, key, source)) = reader {
        if follow.follower_node_id != node_id {
            tracing::warn!(
                target: "hub_directory",
                "follow {follow_id} approved for another library than requested, not put on the catalog readers list"
            );
        } else if let Err(e) = hub_catalog_readers::enrol(db, node_id, &key, source).await {
            tracing::warn!(
                target: "hub_directory",
                "follow {follow_id} approved but not put on the catalog readers list: {e}"
            );
        }
    }
    Ok(FrbHubFollow::from(follow))
}

/// The current catalog key wrapped for `public_hex`, or None when this
/// library holds no catalog key (it pushes in clear) or the key is not a
/// 32-byte hex string.
async fn catalog_key_wrap_for(
    db: &sea_orm::DatabaseConnection,
    public_hex: &str,
) -> Result<Option<String>, String> {
    let Some(key) = catalog_key_store()
        .load(db, &identity_library_uuid()?)
        .await?
    else {
        return Ok(None);
    };
    let Some(public) = hex::decode(public_hex)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
    else {
        return Ok(None);
    };
    key.wrap_for(&public)
        .map(Some)
        .map_err(|e| format!("catalog key wrap: {e}"))
}

/// Lists libraries the local library is following (active follows).
pub async fn hub_directory_list_following() -> Result<Vec<FrbHubFollow>, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .list_following(db)
        .await
        .map(|v| v.into_iter().map(FrbHubFollow::from).collect())
        .map_err(|e| e.to_string())
}

/// An active follower the encrypted catalog cannot be wrapped for keeps it
/// in clear (ADR-072 D8). That must be visible: a key that changed under a
/// pin is a reinstall or a substitution by the hub. Never logs a key.
fn log_reader_anomaly(
    follow: &crate::services::hub_directory_service::HubFollow,
    status: crate::services::hub_catalog_readers::ReaderStatus,
) {
    use crate::services::hub_catalog_readers::{ReaderStatus, short};
    match status {
        ReaderStatus::KeyMismatch => tracing::warn!(
            target: "hub_directory",
            "follow {} ({}): the hub returns another key than the one pinned, the catalog stays in clear",
            follow.id,
            short(&follow.follower_node_id)
        ),
        ReaderStatus::Unknown => tracing::warn!(
            target: "hub_directory",
            "follow {} ({}): active but never let in as a catalog reader, the catalog stays in clear",
            follow.id,
            short(&follow.follower_node_id)
        ),
        ReaderStatus::Reader | ReaderStatus::NotReader => {}
    }
}

/// Lists libraries that follow the local library (active followers), each
/// with where it stands toward the encrypted catalog. Brings the catalog
/// readers list in step on the way (ADR-072 D4): a follower that left is
/// noticed here too, and a failure to do so fails the call, since the
/// statuses would then be unreliable for the clear-or-encrypted decision.
pub async fn hub_directory_list_followers() -> Result<Vec<FrbHubFollow>, String> {
    use crate::services::hub_catalog_readers::{self, CatalogReaders};

    let db = hub_db()?;
    let listed_at = hub_catalog_readers::now_rfc3339();
    let followers = hub_directory_svc()
        .list_followers(db)
        .await
        .map_err(|e| e.to_string())?;
    hub_catalog_readers::reconcile(db, &followers, &listed_at).await?;
    let readers = CatalogReaders::load(db).await?;
    Ok(followers
        .into_iter()
        .map(|f| {
            let status = readers.status(&f);
            if f.status == "active" {
                log_reader_anomaly(&f, status);
            }
            let status = status.as_str().to_string();
            FrbHubFollow {
                catalog_reader_status: Some(status),
                ..FrbHubFollow::from(f)
            }
        })
        .collect())
}

/// Unfollows a library.
pub async fn hub_directory_unfollow(node_id: String) -> Result<(), String> {
    let db = hub_db()?;
    hub_directory_svc()
        .unfollow(db, &node_id)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Hub Borrow Requests FFI (ADR-018)
// ---------------------------------------------------------------------------

/// Creates a hub-mediated borrow request for a book from a followed library.
pub async fn hub_directory_create_borrow_request(
    lender_node_id: String,
    isbn: String,
    book_title: String,
) -> Result<FrbHubBorrowRequest, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .create_borrow_request(db, &lender_node_id, &isbn, &book_title)
        .await
        .map(FrbHubBorrowRequest::from)
        .map_err(|e| e.to_string())
}

/// Fetches incoming borrow requests (pending) for the local library as lender.
pub async fn hub_directory_incoming_borrow_requests() -> Result<Vec<FrbHubBorrowRequest>, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .incoming_borrow_requests(db)
        .await
        .map(|v| v.into_iter().map(FrbHubBorrowRequest::from).collect())
        .map_err(|e| e.to_string())
}

/// Fetches outgoing borrow requests sent by the local library as requester.
pub async fn hub_directory_outgoing_borrow_requests() -> Result<Vec<FrbHubBorrowRequest>, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .outgoing_borrow_requests(db)
        .await
        .map(|v| v.into_iter().map(FrbHubBorrowRequest::from).collect())
        .map_err(|e| e.to_string())
}

/// Resolves a borrow request. resolution: "accept" | "reject"
pub async fn hub_directory_resolve_borrow_request(
    request_id: i64,
    resolution: String,
) -> Result<FrbHubBorrowRequest, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .resolve_borrow_request(db, request_id, &resolution)
        .await
        .map(FrbHubBorrowRequest::from)
        .map_err(|e| e.to_string())
}

/// Cancels a borrow request (requester only).
#[flutter_rust_bridge::frb]
pub async fn hub_directory_cancel_borrow_request(request_id: i64) -> Result<(), String> {
    let db = hub_db()?;
    hub_directory_svc()
        .cancel_borrow_request(db, request_id)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// E2EE Sealed Blob FFI
// ---------------------------------------------------------------------------

/// Encrypts plaintext for a recipient identified by their X25519 public key (hex-encoded).
/// Returns a base64-encoded sealed blob suitable for hub storage.
pub fn seal_blob(recipient_x25519_hex: String, plaintext: String) -> Result<String, String> {
    let key_bytes =
        hex::decode(&recipient_x25519_hex).map_err(|e| format!("Invalid hex key: {e}"))?;
    let key: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "X25519 key must be 32 bytes (64 hex chars)".to_string())?;
    crate::crypto::sealed_blob::seal(&key, plaintext.as_bytes()).map_err(|e| e.to_string())
}

/// Decrypts a base64-encoded sealed blob using the local node identity's X25519 secret key.
/// Returns the plaintext string.
pub async fn open_blob(sealed_base64: String) -> Result<String, String> {
    let svc = IDENTITY_SERVICE
        .get()
        .ok_or("Identity not initialized - call init_identity_ffi first")?;
    let identity = svc.identity()?;
    let static_secret = identity.x25519_static_secret();

    let plaintext_bytes = crate::crypto::sealed_blob::open(static_secret, &sealed_base64)
        .map_err(|e| e.to_string())?;

    String::from_utf8(plaintext_bytes).map_err(|e| format!("UTF-8 decode: {e}"))
}

/// Batch-updates encrypted contact blobs for all active followers.
/// contacts: list of (follow_id, encrypted_contact_base64) pairs.
pub async fn hub_directory_sync_contacts(
    follow_ids: Vec<i64>,
    encrypted_contacts: Vec<String>,
) -> Result<i32, String> {
    if follow_ids.len() != encrypted_contacts.len() {
        return Err("follow_ids and encrypted_contacts must have the same length".to_string());
    }
    let db = hub_db()?;
    let pairs: Vec<(i64, String)> = follow_ids.into_iter().zip(encrypted_contacts).collect();
    hub_directory_svc()
        .sync_follow_contacts(db, &pairs)
        .await
        .map_err(|e| e.to_string())
}

/// Returns the local X25519 public key as hex string, or None if no identity exists.
pub async fn get_local_x25519_public_key() -> Result<Option<String>, String> {
    use sea_orm::ConnectionTrait;
    let db = hub_db()?;
    let backend = db.get_database_backend();
    let row = db
        .query_one(sea_orm::Statement::from_string(
            backend,
            "SELECT public_key FROM crypto_keys WHERE key_type = 'x25519' LIMIT 1".to_owned(),
        ))
        .await
        .map_err(|e| format!("DB error: {e}"))?;

    match row {
        Some(r) => {
            let bytes: Vec<u8> = r
                .try_get("", "public_key")
                .map_err(|e| format!("Failed to read public_key: {e}"))?;
            Ok(Some(hex::encode(bytes)))
        }
        None => Ok(None),
    }
}
