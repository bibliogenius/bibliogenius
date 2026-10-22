//! Peer administration: listing, approval, status, URL, display name, deletion.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use serde_json::json;

/// Bulk-approve all pending peers (called when connection_validation is toggled OFF).
/// A library whose pairing the owner ended keeps waiting: the setting is
/// not about it, and only accepting it one by one restores it (ADR-072 D4).
pub async fn auto_approve_all_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let pending = peer::Entity::find()
        .filter(peer::Column::ConnectionStatus.eq("pending"))
        .all(&db)
        .await
        .unwrap_or_default();

    let mut peers = Vec::with_capacity(pending.len());
    for p in pending {
        if !crate::services::hub_catalog_readers::held_back(&db, &p)
            .await
            .unwrap_or(true)
        {
            peers.push(p);
        }
    }
    let count = peers.len();
    for p in peers {
        let mut active: peer::ActiveModel = p.into();
        active.connection_status = Set("accepted".to_string());
        active.auto_approve = Set(true);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active.update(&db).await;
    }

    tracing::info!("✅ Auto-approved {} pending peers", count);
    (
        StatusCode::OK,
        Json(json!({ "message": format!("Approved {} peers", count), "count": count })),
    )
        .into_response()
}

pub async fn list_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // Legacy hub peer sync removed: peers are managed locally via invite
    // links, QR codes, and mDNS discovery. The old GET /api/peers hub
    // endpoint was causing SQLite lock contention and timeouts on every
    // list_peers call, making peers appear to vanish from the UI.

    let peers = peer::Entity::find().all(&db).await.unwrap_or(vec![]);

    // Convert to JSON with computed status field
    let peers_with_status: Vec<serde_json::Value> = peers
        .into_iter()
        .map(|p| {
            let status = if p.connection_status == "pending" {
                "pending"
            } else {
                "connected"
            };
            json!({
                "id": p.id,
                "name": p.name,
                "display_name": p.display_name,
                "url": p.url,
                "public_key": p.public_key,
                "library_uuid": p.library_uuid,
                "latitude": p.latitude,
                "longitude": p.longitude,
                "auto_approve": p.auto_approve,
                "connection_status": p.connection_status,
                "status": status,
                "relay_url": p.relay_url,
                "mailbox_id": p.mailbox_id,
                "relay_write_token": p.relay_write_token,
                "relay_write_token_invalid_at": p.relay_write_token_invalid_at,
                "last_seen": p.last_seen,
                "avatar_config": p.avatar_config,
                "created_at": p.created_at,
                "updated_at": p.updated_at,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "data": peers_with_status
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct UpdatePeerStatusRequest {
    status: String, // "active" (accept) or "rejected"
}

/// Update a peer's status (accept or reject a connection request)
pub async fn update_peer_status(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerStatusRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // If rejecting, delete the peer entirely
    if payload.status == "rejected" {
        match crate::services::hub_catalog_readers::delete_peer_ending_pairing(&db, &peer).await {
            Ok(()) => {
                // Deactivate Library contacts associated with this peer (matched by name).
                use crate::models::contact;
                let _ = contact::Entity::update_many()
                    .filter(contact::Column::Name.eq(&peer.name))
                    .filter(contact::Column::Type.eq("Library"))
                    .col_expr(
                        contact::Column::IsActive,
                        sea_orm::sea_query::Expr::value(false),
                    )
                    .col_expr(
                        contact::Column::UpdatedAt,
                        sea_orm::sea_query::Expr::value(chrono::Utc::now().to_rfc3339()),
                    )
                    .exec(&db)
                    .await;
                // A rejected peer leaves the catalog readers like any ended
                // pairing (ADR-072 D4); its wrap goes at once.
                withdraw_catalog_key_for_peer(
                    std::sync::Arc::new(
                        crate::services::hub_directory_service::HubDirectoryService::new(),
                    ),
                    db.clone(),
                    &peer,
                );
                tracing::info!("🗑️ Peer {} rejected and deleted", peer_id);
                return (
                    StatusCode::OK,
                    Json(json!({
                        "message": "Peer rejected and removed",
                        "peer_id": peer_id
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
                )
                    .into_response();
            }
        }
    }

    // Update auto_approve and connection_status for accept/active
    let auto_approve = payload.status == "active" || payload.status == "accepted";
    let restore = auto_approve.then(|| (peer.library_uuid.clone(), peer.public_key.clone()));
    let mut active_model: peer::ActiveModel = peer.into();
    active_model.auto_approve = Set(auto_approve);
    if auto_approve {
        active_model.connection_status = Set("accepted".to_string());
    }
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    // Accepting a library whose pairing was ended is the owner restoring it
    // (ADR-072 D4).
    let accepted = crate::services::hub_catalog_readers::update_peer_restoring_pairing(
        &db,
        active_model,
        restore,
    )
    .await;

    match accepted {
        Ok(updated) => {
            tracing::info!("Peer {} accepted, auto_approve={}", peer_id, auto_approve);

            // Emit connection_accepted notification
            if auto_approve {
                crate::services::notification_service::emit(
                    &db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::ConnectionAccepted,
                        title: updated.name.clone(),
                        body: None,
                        ref_type: Some("peer".to_string()),
                        ref_id: Some(peer_id.to_string()),
                    },
                )
                .await;
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer accepted",
                    "peer": updated,
                    "auto_approve": auto_approve
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdatePeerUrlRequest {
    pub url: String,
    /// Optional library_uuid to backfill when discovered via mDNS.
    /// Validated as a proper UUID to prevent injection.
    pub library_uuid: Option<String>,
}

/// Update a peer's URL (for mDNS IP changes)
/// Security: Only pending peers can have their URL updated
pub async fn update_peer_url(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerUrlRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // The app may match this row to an mDNS announcement by name, which any
    // host can take: a uuid that contradicts the stored one is another
    // library, and nothing of this row moves to its address. Checked before
    // any holder of that address is touched.
    if let (Some(stored), Some(announced)) = (&peer.library_uuid, &payload.library_uuid)
        && !stored.eq_ignore_ascii_case(announced)
    {
        tracing::warn!(
            "update_peer_url: peer {} announced with another library uuid, keeping its row",
            peer_id
        );
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "Another library announced this address" })),
        )
            .into_response();
    }

    // Security: Only update URL for pending peers, unless upgrading from relay
    // to LAN or fixing a port mismatch (mDNS discovered the correct address).
    // This endpoint is localhost-only, so the caller is always the local app.
    if peer.auto_approve && !peer.url.starts_with("relay://") {
        // Allow port updates for same-host LAN URLs (hot restart changes port)
        let same_host = match (url::Url::parse(&peer.url), url::Url::parse(&payload.url)) {
            (Ok(old), Ok(new_url)) => old.host() == new_url.host(),
            _ => false,
        };
        if !same_host {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Cannot update URL for connected peers" })),
            )
                .into_response();
        }
    }

    // Check if URL is already taken by another peer
    if let Ok(Some(existing_peer)) = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .filter(peer::Column::Id.ne(peer_id))
        .one(&db)
        .await
    {
        // A pending row holding this URL is a stale entry (e.g. from a previous
        // mDNS discovery on this IP) and is deleted to free the URL. A paired
        // peer or a catalog reader is never deleted here: this is no owner
        // gesture, so it must not end a pairing nor leave a reader without a
        // row (ADR-072 D4). An unreadable list counts as a reader.
        let reader = crate::services::hub_catalog_readers::is_current_reader(
            &db,
            existing_peer.library_uuid.as_deref(),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                "update_peer_url: catalog readers unreadable, peer {} keeps {}: {e}",
                existing_peer.id,
                payload.url
            );
            true
        });
        if !existing_peer.auto_approve && existing_peer.connection_status != "accepted" && !reader {
            tracing::info!(
                "♻️ deleting stale peer {} to free up URL {}",
                existing_peer.id,
                payload.url
            );
            let _ = peer::Entity::delete_by_id(existing_peer.id).exec(&db).await;
        } else {
            // If it's an approved peer, we can't just delete it.
            // This is a genuine conflict (two trusted peers on same IP? or same peer different ID?)
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "URL already in use by another trusted peer" })),
            )
                .into_response();
        }
    }

    let peer_has_no_uuid = peer.library_uuid.is_none();
    let mut active_model: peer::ActiveModel = peer.into();
    active_model.url = Set(payload.url.clone());
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    // Backfill library_uuid if missing, provided and valid UUID format
    if peer_has_no_uuid && let Some(ref uuid_str) = payload.library_uuid {
        if uuid::Uuid::parse_str(uuid_str).is_ok() {
            active_model.library_uuid = Set(Some(uuid_str.clone()));
            tracing::info!(
                "Backfilling library_uuid for peer {}: {}",
                peer_id,
                uuid_str
            );
        } else {
            tracing::warn!(
                "Ignoring invalid library_uuid for peer {}: {}",
                peer_id,
                uuid_str
            );
        }
    }

    match active_model.update(&db).await {
        Ok(updated) => {
            tracing::info!("✅ Peer {} URL updated to: {}", peer_id, payload.url);
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer URL updated",
                    "peer": updated
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

/// Withdraws, in the background, the catalog key wrap the ex-peer holds on
/// its follow (ADR-072 D4). Withdrawn whether or not it was on the readers
/// list: a wrap handed out before the list existed must go too. The
/// rotation at the next encrypted push covers a failure.
pub(crate) fn withdraw_catalog_key_for_peer(
    hub_directory: std::sync::Arc<crate::services::hub_directory_service::HubDirectoryService>,
    db: DatabaseConnection,
    peer_model: &peer::Model,
) {
    use crate::services::hub_directory_service::HubDirectoryError;

    let Some(node_id) = peer_model
        .library_uuid
        .clone()
        .filter(|u| !u.trim().is_empty())
    else {
        return;
    };
    let peer_id = peer_model.id;
    tokio::spawn(async move {
        match hub_directory.withdraw_catalog_key_for(&db, &node_id).await {
            // Not on the hub: no follow, no wrap.
            Ok(_) | Err(HubDirectoryError::NotRegistered) => {}
            Err(e) => tracing::warn!(
                target: "hub_directory",
                "catalog key wrap not withdrawn for removed peer {peer_id}, the next rotation covers it: {e}"
            ),
        }
    });
}

/// Removes cached hub directory catalog entries (peer_id = 0 sentinel) for a
/// given library_uuid. See ADR-024: the cache is owned by the peer relationship,
/// so deletion must invalidate it to prevent stale reads on re-add.
/// ADR-053 follow-up: the hub follow that materialised the pairing is revoked
/// once the local `peers` row is gone, on every path that removes it (own
/// deletion here, and both disconnect receivers). Fire-and-forget, like the
/// disconnect notification: a hub outage never blocks the local removal.
/// The catalog key wrap of the ex-peer is withdrawn on the way (ADR-072 D4).
pub(crate) fn revoke_hub_follow_for_peer(
    state: &crate::infrastructure::AppState,
    peer_model: &peer::Model,
) {
    use crate::services::hub_directory_service::FollowRevocation;

    let library_uuid = peer_model.library_uuid.clone();
    let peer_id = peer_model.id;
    withdraw_catalog_key_for_peer(state.hub_directory.clone(), state.db().clone(), peer_model);
    let state = state.clone();
    tokio::spawn(async move {
        match state
            .hub_directory
            .revoke_follow_for_peer(state.db(), library_uuid.as_deref())
            .await
        {
            FollowRevocation::Revoked => {
                tracing::info!("Hub follow revoked for removed peer {}", peer_id)
            }
            FollowRevocation::Skipped => {}
            FollowRevocation::Failed(e) => tracing::warn!(
                "Hub follow revocation failed for removed peer {}: {}",
                peer_id,
                e
            ),
        }
    });
}

async fn purge_hub_catalog_cache(db: &DatabaseConnection, library_uuid: &str) {
    use crate::models::peer_book;
    match peer_book::Entity::delete_many()
        .filter(peer_book::Column::NodeId.eq(library_uuid))
        .filter(peer_book::Column::PeerId.eq(0))
        .exec(db)
        .await
    {
        Ok(res) => tracing::info!(
            "Purged {} hub catalog cache entries for library_uuid={}",
            res.rows_affected,
            library_uuid
        ),
        Err(e) => tracing::warn!(
            "Failed to purge hub catalog cache for library_uuid={}: {}",
            library_uuid,
            e
        ),
    }
}

pub async fn delete_peer(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Load peer before deletion so we can notify the remote side
    let peer_model = match peer::Entity::find_by_id(peer_id).one(db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Notify remote peer (fire-and-forget, never blocks local deletion)
    let state_clone = state.clone();
    let peer_clone = peer_model.clone();
    tokio::spawn(async move {
        notify_peer_of_disconnect(&state_clone, &peer_clone).await;
    });

    // 3. Delete locally
    match crate::services::hub_catalog_readers::delete_peer_ending_pairing(db, &peer_model).await {
        Ok(()) => {
            // Deactivate Library contacts associated with this peer (matched by name).
            // These contacts were auto-created during P2P interactions and are stale
            // now that the peer connection is gone.
            use crate::models::contact;
            let _ = contact::Entity::update_many()
                .filter(contact::Column::Name.eq(&peer_model.name))
                .filter(contact::Column::Type.eq("Library"))
                .col_expr(
                    contact::Column::IsActive,
                    sea_orm::sea_query::Expr::value(false),
                )
                .col_expr(
                    contact::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(chrono::Utc::now().to_rfc3339()),
                )
                .exec(db)
                .await;
            // ADR-024: purge the hub directory catalog cache for this peer's
            // library_uuid so re-adding the same peer does not serve stale entries.
            if let Some(ref uuid) = peer_model.library_uuid {
                purge_hub_catalog_cache(db, uuid).await;
            }
            revoke_hub_follow_for_peer(&state, &peer_model);
            tracing::info!("🗑️ Peer {} ({}) deleted", peer_id, peer_model.name);
            (StatusCode::OK, Json(json!({ "message": "Peer deleted" }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdatePeerDisplayNameRequest {
    pub display_name: String,
}

/// Update a peer's user-defined display name.
pub async fn update_peer_display_name(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerDisplayNameRequest>,
) -> impl IntoResponse {
    let peer_opt = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    let peer_model = match peer_opt {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    let display_name = payload.display_name.trim().to_string();
    let mut active: peer::ActiveModel = peer_model.into();
    active.display_name = Set(if display_name.is_empty() {
        None
    } else {
        Some(display_name)
    });
    active.updated_at = Set(Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(updated) => (StatusCode::OK, Json(json!({ "peer": updated }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update display name: {}", e) })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod hub_catalog_cache_tests {
    use super::*;
    use crate::db;
    use crate::models::peer_book;
    use sea_orm::{ConnectionTrait, Set, Statement};

    async fn setup_cache_db() -> DatabaseConnection {
        let db = db::init_db("sqlite::memory:").await.expect("init db");
        // Directory cache uses peer_id = 0 sentinel (no matching peer row), same
        // workaround as upsert_directory_catalog_cache in api/frb/hub_catalog.rs.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();
        db
    }

    async fn insert_cache_entry(db: &DatabaseConnection, node_id: &str, isbn: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        let pb = peer_book::ActiveModel {
            peer_id: Set(0), // sentinel for directory entries
            remote_book_id: Set("0".to_string()),
            title: Set(format!("Book {}", isbn)),
            isbn: Set(Some(isbn.to_string())),
            author: Set(None),
            cover_url: Set(None),
            summary: Set(None),
            synced_at: Set(now),
            node_id: Set(Some(node_id.to_string())),
            first_seen_at: Set(None),
            added_at: Set(None),
            notified_at: Set(None),
            ..Default::default()
        };
        peer_book::Entity::insert(pb).exec(db).await.unwrap();
    }

    /// ADR-024: purging the cache for a library_uuid must drop all sentinel
    /// directory entries for that node_id, and only those.
    #[tokio::test]
    async fn purge_hub_catalog_cache_removes_only_matching_node_id() {
        let db = setup_cache_db().await;
        let target = "41610ad0-d659-4b09-8303-faacf9e6aa36";
        let other = "26e4b4d9-acff-42cb-8b25-0bf32457a232";

        insert_cache_entry(&db, target, "978-target-1").await;
        insert_cache_entry(&db, target, "978-target-2").await;
        insert_cache_entry(&db, other, "978-other-1").await;

        purge_hub_catalog_cache(&db, target).await;

        let remaining = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(0))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "only the other node's entry should remain"
        );
        assert_eq!(remaining[0].node_id.as_deref(), Some(other));
    }

    /// Purge must be a no-op when there is nothing to remove for the given uuid.
    #[tokio::test]
    async fn purge_hub_catalog_cache_no_op_when_empty() {
        let db = setup_cache_db().await;
        let other = "26e4b4d9-acff-42cb-8b25-0bf32457a232";
        insert_cache_entry(&db, other, "978-other-1").await;

        purge_hub_catalog_cache(&db, "unknown-uuid").await;

        let remaining = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(0))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }
}

/// Which row `update_peer_url` may delete to free an address: a stale
/// pending row, never a paired peer nor a catalog reader. A deletion here is
/// no owner gesture, so it must not end a pairing behind the readers list
/// (ADR-072 D4).
#[cfg(test)]
mod update_peer_url_tests {
    use super::*;
    use crate::db;
    use crate::services::hub_catalog_readers::{self, ReaderSource};

    const TAKEN_URL: &str = "http://192.168.1.30:8000";
    const UUID_HOLDER: &str = "5f0c2a8e-3b1d-4c7a-9e2f-8d6b4a1c0e93";
    const KEY_HOLDER_X: &str = "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1";

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer(
        db: &DatabaseConnection,
        url: &str,
        library_uuid: Option<&str>,
        status: &str,
        auto_approve: bool,
    ) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set(format!("peer at {url}")),
            url: Set(url.to_string()),
            library_uuid: Set(library_uuid.map(str::to_string)),
            connection_status: Set(status.to_string()),
            auto_approve: Set(auto_approve),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer")
        .id
    }

    async fn move_to_taken_url(db: &DatabaseConnection, peer_id: i32) -> StatusCode {
        update_peer_url(
            State(db.clone()),
            Path(peer_id),
            Json(UpdatePeerUrlRequest {
                url: TAKEN_URL.to_string(),
                library_uuid: None,
            }),
        )
        .await
        .into_response()
        .status()
    }

    async fn url_of(db: &DatabaseConnection, peer_id: i32) -> Option<String> {
        peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .expect("find peer")
            .map(|p| p.url)
    }

    async fn uuid_of(db: &DatabaseConnection, peer_id: i32) -> Option<String> {
        peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .expect("find peer")
            .and_then(|p| p.library_uuid)
    }

    async fn move_with_uuid(
        db: &DatabaseConnection,
        peer_id: i32,
        url: &str,
        uuid: &str,
    ) -> StatusCode {
        update_peer_url(
            State(db.clone()),
            Path(peer_id),
            Json(UpdatePeerUrlRequest {
                url: url.to_string(),
                library_uuid: Some(uuid.to_string()),
            }),
        )
        .await
        .into_response()
        .status()
    }

    #[tokio::test]
    async fn an_unreadable_readers_list_keeps_the_holder() {
        use sea_orm::{ConnectionTrait, Statement};
        let db = setup_db().await;
        let holder = insert_peer(&db, TAKEN_URL, Some(UUID_HOLDER), "pending", false).await;
        let mover = insert_peer(&db, "relay://mover", None, "pending", false).await;
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE catalog_readers RENAME TO catalog_readers_unreadable".to_owned(),
        ))
        .await
        .expect("hide the readers list");

        assert_eq!(move_to_taken_url(&db, mover).await, StatusCode::CONFLICT);
        assert_eq!(url_of(&db, holder).await.as_deref(), Some(TAKEN_URL));
    }

    /// The app matches a relay peer to an mDNS announcement by name, and any
    /// host can take that name: the uuid it announces must never replace the
    /// one the row holds, nor move the row onto its address.
    #[tokio::test]
    async fn a_stored_uuid_is_never_replaced_by_another() {
        let db = setup_db().await;
        let other_uuid = "0b7e4c1a-9d2f-4e83-a6c5-1f8d3b2a7e40";
        let peer = insert_peer(&db, "relay://holder", Some(UUID_HOLDER), "pending", false).await;

        assert_eq!(
            move_with_uuid(&db, peer, TAKEN_URL, other_uuid).await,
            StatusCode::CONFLICT
        );
        assert_eq!(uuid_of(&db, peer).await.as_deref(), Some(UUID_HOLDER));
        assert_eq!(url_of(&db, peer).await.as_deref(), Some("relay://holder"));
    }

    #[tokio::test]
    async fn a_missing_uuid_is_backfilled_and_the_same_one_is_accepted() {
        let db = setup_db().await;
        let blank = insert_peer(&db, "relay://blank", None, "pending", false).await;
        assert_eq!(
            move_with_uuid(&db, blank, TAKEN_URL, UUID_HOLDER).await,
            StatusCode::OK
        );
        assert_eq!(uuid_of(&db, blank).await.as_deref(), Some(UUID_HOLDER));

        let known_uuid = "0b7e4c1a-9d2f-4e83-a6c5-1f8d3b2a7e40";
        let known = insert_peer(&db, "relay://known", Some(known_uuid), "pending", false).await;
        assert_eq!(
            move_with_uuid(
                &db,
                known,
                "http://192.168.1.31:8000",
                &known_uuid.to_uppercase()
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(uuid_of(&db, known).await.as_deref(), Some(known_uuid));
    }

    #[tokio::test]
    async fn a_stale_pending_row_gives_up_its_address() {
        let db = setup_db().await;
        let holder = insert_peer(&db, TAKEN_URL, None, "pending", false).await;
        let mover = insert_peer(&db, "relay://mover", None, "pending", false).await;

        assert_eq!(move_to_taken_url(&db, mover).await, StatusCode::OK);
        assert_eq!(url_of(&db, holder).await, None, "stale row deleted");
        assert_eq!(url_of(&db, mover).await.as_deref(), Some(TAKEN_URL));
    }

    #[tokio::test]
    async fn an_accepted_peer_without_auto_approve_keeps_its_address() {
        let db = setup_db().await;
        let holder = insert_peer(&db, TAKEN_URL, Some(UUID_HOLDER), "accepted", false).await;
        let mover = insert_peer(&db, "relay://mover", None, "pending", false).await;

        assert_eq!(move_to_taken_url(&db, mover).await, StatusCode::CONFLICT);
        assert_eq!(url_of(&db, holder).await.as_deref(), Some(TAKEN_URL));
        assert_eq!(url_of(&db, mover).await.as_deref(), Some("relay://mover"));
    }

    #[tokio::test]
    async fn a_pending_row_still_reading_the_catalog_keeps_its_address() {
        let db = setup_db().await;
        let holder = insert_peer(&db, TAKEN_URL, Some(UUID_HOLDER), "pending", false).await;
        assert!(
            hub_catalog_readers::enrol(&db, UUID_HOLDER, KEY_HOLDER_X, ReaderSource::Approval)
                .await
                .expect("enrol")
        );
        let mover = insert_peer(&db, "relay://mover", None, "pending", false).await;

        assert_eq!(move_to_taken_url(&db, mover).await, StatusCode::CONFLICT);
        assert_eq!(url_of(&db, holder).await.as_deref(), Some(TAKEN_URL));
        assert!(
            hub_catalog_readers::is_current_reader(&db, Some(UUID_HOLDER))
                .await
                .expect("readers"),
            "the reader stays on the list"
        );
    }
}
