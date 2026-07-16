//! Peer catalog synchronization and operation push/pull.

use super::*;
use crate::models::{operation_log, peer, peer_book, peer_gamification_stats};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Upsert peer books cache: stores `added_at` from the owner peer so the
/// "new" badge is consistent across all viewers (no longer derived from
/// the local cache observation time). Returns the number of books in the
/// fresh list.
///
/// `is_full_snapshot` controls the delete-absent pass: when `true` the batch
/// is the peer's complete catalog and rows missing from it are deleted (the
/// owner removed those books). When `false` the batch is only a subset (a
/// single paginated page, or a relay page loop cut short by a timeout), so the
/// upsert is purely additive — deleting absent rows would drain the cache and
/// leave the viewer staring at "no books".
pub(crate) async fn upsert_peer_books_cache(
    db: &DatabaseConnection,
    peer_id: i32,
    node_id: Option<&str>,
    books: Vec<crate::models::Book>,
    is_full_snapshot: bool,
) -> usize {
    let now = chrono::Utc::now().to_rfc3339();
    let count = books.len();

    tracing::info!(
        "upsert_peer_books_cache: peer_id={}, incoming={} books",
        peer_id,
        count,
    );

    // 1. Load existing cached books for this peer
    let mut existing = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(db)
        .await
        .unwrap_or_default();

    // Migration: a previous bug in Book.toJson() omitted the id field, causing
    // all cached entries to have remote_book_id=0. When incoming books now carry
    // real IDs, purge the corrupted rows so the fresh upsert replaces them.
    let zero_id_count = existing
        .iter()
        .filter(|e| e.remote_book_id.is_empty())
        .count();
    let incoming_have_real_ids = books
        .iter()
        .any(|b| matches!(&b.id, Some(id) if !id.is_empty()));
    if zero_id_count > 1 && incoming_have_real_ids {
        tracing::info!(
            "upsert_peer_books_cache: peer_id={} - purging {} corrupted entries \
             (remote_book_id=0) from previous toJson bug",
            peer_id,
            zero_id_count,
        );
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(""))
            .exec(db)
            .await;
        existing.retain(|e| !e.remote_book_id.is_empty());
    }

    let existing_map: std::collections::HashMap<String, peer_book::Model> = existing
        .into_iter()
        .map(|e| (e.remote_book_id.clone(), e))
        .collect();

    let mut fresh_ids = std::collections::HashSet::new();

    // Guard: if incoming list is empty but we have cached books, this would
    // wipe the entire cache. Log a warning to help diagnose relay truncation.
    if count == 0 && !existing_map.is_empty() {
        tracing::warn!(
            "upsert_peer_books_cache: peer_id={} - incoming=0 but {} cached books exist, \
             skipping destructive sync (possible relay truncation)",
            peer_id,
            existing_map.len(),
        );
        return 0;
    }

    // 2. Upsert each book
    for book in books {
        let remote_id = book.id.unwrap_or_default();
        fresh_ids.insert(remote_id.clone());

        if let Some(existing_entry) = existing_map.get(&remote_id) {
            // UPDATE: refresh metadata. `added_at` from the peer overrides any
            // stale local value (the owner is the source of truth).
            let mut active: peer_book::ActiveModel = existing_entry.clone().into();
            active.title = Set(book.title);
            active.isbn = Set(book.isbn);
            active.author = Set(book.author);
            active.cover_url = Set(book.cover_url);
            active.summary = Set(book.summary);
            active.synced_at = Set(now.clone());
            if let Some(nid) = node_id {
                active.node_id = Set(Some(nid.to_string()));
            }
            if book.added_at.is_some() {
                active.added_at = Set(book.added_at);
            }
            // Loan status: owner is authoritative. Missing `owned` means a
            // pre-073 peer or a DTO with the field stripped — fall back to
            // `true` so legacy books stay visible.
            active.owned = Set(book.owned.unwrap_or(true));
            active.available_copies = Set(book.available_copies);
            // notified_at stays unchanged
            let _ = active.update(db).await;
        } else {
            // INSERT: new book (notified_at = NULL - not yet notified)
            let cache = peer_book::ActiveModel {
                peer_id: Set(peer_id),
                remote_book_id: Set(remote_id),
                title: Set(book.title),
                isbn: Set(book.isbn),
                author: Set(book.author),
                cover_url: Set(book.cover_url),
                summary: Set(book.summary),
                synced_at: Set(now.clone()),
                node_id: Set(node_id.map(|s| s.to_string())),
                first_seen_at: Set(None),
                added_at: Set(book.added_at),
                notified_at: Set(None),
                owned: Set(book.owned.unwrap_or(true)),
                available_copies: Set(book.available_copies),
                ..Default::default()
            };
            let _ = peer_book::Entity::insert(cache).exec(db).await;
        }
    }

    // 3. Delete books no longer in the fresh list — ONLY when this batch is a
    // complete snapshot. A partial fetch lists a subset, so pruning everything
    // absent from it would wipe books the peer still owns (cache drain).
    if is_full_snapshot {
        for (remote_id, entry) in &existing_map {
            if !fresh_ids.contains(remote_id) {
                let _ = peer_book::Entity::delete_by_id(entry.id).exec(db).await;
            }
        }
    }

    // 4. Check un-notified books against wishlist + emit "wishlist_match"
    // notification. Uses notified_at IS NULL instead of tracking inserts in
    // memory, so that notification dedup survives notification pruning (TTL/cap).
    // Only books the peer actually owns qualify: a non-owned entry (the peer's
    // own wishlist, or a copy the peer borrowed) is not borrowable, so it stays
    // un-notified and becomes eligible if the peer acquires it later.
    let unnotified = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .filter(peer_book::Column::Owned.eq(true))
        .filter(peer_book::Column::NotifiedAt.is_null())
        .all(db)
        .await
        .unwrap_or_default();

    if !unnotified.is_empty() {
        let new_isbns: Vec<(String, String)> = unnotified
            .iter()
            .filter_map(|pb| {
                pb.isbn
                    .as_ref()
                    .map(|isbn| (isbn.clone(), pb.title.clone()))
            })
            .collect();

        let peer_name = peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default();
        let ref_id = peer_id.to_string();

        // Wishlist matches
        if !new_isbns.is_empty() {
            crate::services::notification_service::check_wishlist_matches(
                db, &new_isbns, &peer_name, "peer", &ref_id,
            )
            .await;
        }

        // Mark all un-notified books as notified so they won't trigger again
        for pb in unnotified {
            let mut active: peer_book::ActiveModel = pb.into();
            active.notified_at = Set(Some(now.clone()));
            let _ = active.update(db).await;
        }
    }

    count
}

/// Internal sync function for background sync after connect
pub(crate) async fn sync_peer_internal(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
) -> Result<usize, String> {
    // Validate URL
    validate_url(peer_url).map_err(|e| format!("Invalid peer URL: {}", e))?;

    let client = get_safe_client();

    // First, check peer's config for privacy consent flags
    let config_url = format!("{}/api/config", peer_url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };

    // Distinguish "peer explicitly disallows caching" from "peer unreachable"
    let peer_reachable = peer_config.is_some();
    let allows_caching = peer_config
        .as_ref()
        .map(|c| c.allow_library_caching)
        .unwrap_or(true); // assume caching OK when unreachable - preserve cache
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));

    // Extract updated name and avatar from peer config (single DB read)
    let peer_library_name = peer_config.as_ref().map(|c| c.library_name.clone());
    let (updated_name, updated_avatar) = if let Some(config) = &peer_config {
        if let Ok(Some(p)) = peer::Entity::find_by_id(peer_id).one(db).await {
            let name = if p.name != config.library_name {
                Some(config.library_name.clone())
            } else {
                None
            };
            let avatar_json = config
                .avatar_config
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let avatar = if avatar_json != p.avatar_config {
                avatar_json
            } else {
                None
            };
            (name, avatar)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Resolve peer display name for memory score upsert
    let display_name = peer_library_name.as_deref().unwrap_or(peer_url);

    if peer_reachable && !allows_caching {
        tracing::info!(
            "Peer {} explicitly disallows library caching, clearing cache",
            peer_url
        );
        // Peer is reachable and explicitly disallows caching - clear cache
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .exec(db)
            .await;
        // Still sync gamification stats if available
        sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;
        // Still sync memory game scores
        crate::modules::memory_game::handlers::sync_peer_memory_scores(
            db,
            peer_id,
            peer_url,
            display_name,
            &client,
            peer_has_memory_game,
        )
        .await;
        // Still sync sliding puzzle scores
        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
            db,
            peer_id,
            peer_url,
            display_name,
            &client,
            peer_has_sliding_puzzle,
        )
        .await;
        // Still update last_seen (and name if changed)
        if let Ok(Some(peer)) = crate::models::peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
        {
            let mut active_peer: peer::ActiveModel = peer.into();
            if let Some(ref new_name) = updated_name {
                active_peer.name = Set(new_name.clone());
                tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
            }
            if let Some(ref avatar) = updated_avatar {
                active_peer.avatar_config = Set(Some(avatar.clone()));
            }
            active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active_peer.update(db).await;
        }
        return Ok(0); // Return 0 books cached
    }

    // Fetch remote books (owned only - exclude books the peer borrowed from others)
    let url = format!("{}/api/books?owned_only=true", peer_url);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to contact peer: {}", e))?;

    if !response.status().is_success() {
        return Err("Peer returned error".to_string());
    }

    // Parse response
    #[derive(Deserialize)]
    struct BooksResponse {
        books: Vec<crate::models::Book>,
    }

    let data: BooksResponse = response
        .json()
        .await
        .map_err(|_| "Invalid response format".to_string())?;

    // Upsert books cache (preserves first_seen_at for existing entries).
    // `/api/books?owned_only=true` returns the peer's full catalog, so this is
    // a complete snapshot: prune books the peer no longer owns.
    let count = upsert_peer_books_cache(db, peer_id, None, data.books, true).await;

    // Sync gamification stats if both sides have the module enabled
    sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;

    // Sync memory game scores
    crate::modules::memory_game::handlers::sync_peer_memory_scores(
        db,
        peer_id,
        peer_url,
        display_name,
        &client,
        peer_has_memory_game,
    )
    .await;

    // Sync sliding puzzle scores
    crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
        db,
        peer_id,
        peer_url,
        display_name,
        &client,
        peer_has_sliding_puzzle,
    )
    .await;

    // Update peer's last_seen (and name if changed)
    if let Ok(Some(peer)) = crate::models::peer::Entity::find_by_id(peer_id)
        .one(db)
        .await
    {
        let mut active_peer: peer::ActiveModel = peer.into();
        if let Some(ref new_name) = updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        if let Some(ref avatar) = updated_avatar {
            active_peer.avatar_config = Set(Some(avatar.clone()));
        }
        active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active_peer.update(db).await;
    }

    tracing::info!(
        "✅ Background sync completed: {} books cached for peer {}",
        count,
        peer_id
    );
    Ok(count)
}

/// Sync gamification stats from a peer.
/// `peer_shares_stats`:
///   - `Some(true)`:  peer confirmed it shares → fetch fresh stats
///   - `Some(false)`: peer confirmed it does NOT share → delete cached stats
///   - `None`:        peer was unreachable (config unknown) → preserve cache, skip sync
pub(crate) async fn sync_peer_gamification_stats(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
    client: &reqwest::Client,
    peer_shares_stats: Option<bool>,
) {
    use crate::models::installation_profile;

    // Check if network_gamification is enabled locally
    let local_enabled = match installation_profile::Entity::find_by_id(1).one(db).await {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"network_gamification".to_string())
        }
        _ => false,
    };

    if !local_enabled {
        return;
    }

    match peer_shares_stats {
        None => {
            // Peer unreachable — preserve cached data
            tracing::debug!(
                "Peer {} config unknown, preserving cached gamification stats",
                peer_url
            );
            return;
        }
        Some(false) => {
            // Peer explicitly does NOT share stats — clean up cache
            let _ = peer_gamification_stats::Entity::delete_many()
                .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
                .exec(db)
                .await;
            return;
        }
        Some(true) => {} // Peer shares — continue to fetch
    }

    // Fetch peer's public gamification stats
    let stats_url = format!("{}/api/gamification/public-stats", peer_url);
    let stats = match client.get(&stats_url).send().await {
        Ok(res) if res.status().is_success() => {
            match res
                .json::<crate::api::gamification::PublicGamificationStats>()
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to parse gamification stats from peer: {}", e);
                    return;
                }
            }
        }
        _ => {
            tracing::warn!("Failed to fetch gamification stats from peer {}", peer_url);
            return;
        }
    };

    // Upsert: delete old + insert new (same pattern as peer_books)
    let _ = peer_gamification_stats::Entity::delete_many()
        .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
        .exec(db)
        .await;

    let entry = peer_gamification_stats::ActiveModel {
        peer_id: Set(peer_id),
        library_name: Set(stats.library_name),
        collector_level: Set(stats.collector.level),
        collector_current: Set(stats.collector.current as i32),
        reader_level: Set(stats.reader.level),
        reader_current: Set(stats.reader.current as i32),
        lender_level: Set(stats.lender.level),
        lender_current: Set(stats.lender.current as i32),
        cataloguer_level: Set(stats.cataloguer.level),
        cataloguer_current: Set(stats.cataloguer.current as i32),
        synced_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    if let Err(e) = peer_gamification_stats::Entity::insert(entry)
        .exec(db)
        .await
    {
        tracing::warn!("Failed to save peer gamification stats: {}", e);
    } else {
        tracing::info!("Gamification stats synced for peer {}", peer_id);
    }
}

#[derive(Deserialize)]
pub struct PushRequest {
    operations: Vec<OperationDto>,
}

#[derive(Serialize, Deserialize)]
pub struct OperationDto {
    entity_type: String,
    entity_id: String,
    operation: String,
    payload: Option<String>,
    created_at: String,
}

pub async fn push_operations(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<PushRequest>,
) -> impl IntoResponse {
    // Simplified: just log them for now, in real app we'd apply them
    for op in payload.operations {
        let log = operation_log::ActiveModel {
            entity_type: Set(op.entity_type),
            entity_id: Set(op.entity_id),
            operation: Set(op.operation),
            payload: Set(op.payload),
            created_at: Set(op.created_at),
            ..Default::default()
        };
        let _ = operation_log::Entity::insert(log).exec(&db).await;
    }
    (
        StatusCode::OK,
        Json(json!({ "message": "Operations received" })),
    )
        .into_response()
}

pub async fn pull_operations(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let ops = operation_log::Entity::find()
        .all(&db)
        .await
        .unwrap_or(vec![]);
    (StatusCode::OK, Json(ops)).into_response()
}

pub async fn sync_peer(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    // Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // 2. Validate URL and fetch remote books
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();

    // Check peer config for gamification sharing
    let config_url = format!("{}/api/config", peer.url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));
    let peer_display_name = peer_config
        .as_ref()
        .map(|c| c.library_name.clone())
        .unwrap_or_else(|| peer.name.clone());

    // Update library_uuid: backfill if missing, or detect changes (peer reset).
    // Validates UUID format to prevent a malicious peer from injecting arbitrary strings.
    if let Some(remote_uuid) = peer_config.as_ref().and_then(|c| c.library_uuid.clone()) {
        if uuid::Uuid::parse_str(&remote_uuid).is_ok() {
            let uuid_changed = peer
                .library_uuid
                .as_ref()
                .is_some_and(|old| old != &remote_uuid);
            let uuid_missing = peer.library_uuid.is_none();

            if uuid_changed || uuid_missing {
                let mut active: peer::ActiveModel = peer.clone().into();
                active.library_uuid = Set(Some(remote_uuid.clone()));
                if let Err(e) = active.update(&db).await {
                    tracing::warn!("Failed to update library_uuid for peer {}: {}", peer_id, e);
                } else if uuid_changed {
                    // Peer was reset/reinstalled - upsert_peer_books_cache will
                    // handle the transition atomically (insert new, update
                    // existing, delete absent). No premature cache wipe needed.
                    tracing::info!(
                        "Peer {} library_uuid changed during sync, will refresh via upsert",
                        peer_id
                    );
                } else {
                    tracing::info!("Backfilled library_uuid for peer {}", peer_id);
                }
            }
        } else {
            tracing::warn!("Peer {} sent invalid library_uuid, ignoring", peer_id);
        }
    }

    let url = format!("{}/api/books?owned_only=true", peer.url);

    let res = client.get(&url).send().await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // Parse response: { "books": [...] }
                #[derive(Deserialize)]
                struct BooksResponse {
                    books: Vec<crate::models::Book>,
                }

                match response.json::<BooksResponse>().await {
                    Ok(data) => {
                        // Upsert books cache (preserves first_seen_at).
                        // Full `/api/books?owned_only=true` catalog → snapshot.
                        let count =
                            upsert_peer_books_cache(&db, peer.id, None, data.books, true).await;

                        // Sync gamification stats
                        sync_peer_gamification_stats(
                            &db,
                            peer.id,
                            &peer.url,
                            &client,
                            shares_gamification,
                        )
                        .await;

                        // Sync memory game scores
                        crate::modules::memory_game::handlers::sync_peer_memory_scores(
                            &db,
                            peer.id,
                            &peer.url,
                            &peer_display_name,
                            &client,
                            peer_has_memory_game,
                        )
                        .await;

                        // Sync sliding puzzle scores
                        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
                            &db,
                            peer.id,
                            &peer.url,
                            &peer_display_name,
                            &client,
                            peer_has_sliding_puzzle,
                        )
                        .await;

                        (
                            StatusCode::OK,
                            Json(json!({ "message": "Sync successful", "count": count })),
                        )
                            .into_response()
                    }
                    _ => (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "Invalid response format" })),
                    )
                        .into_response(),
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned error" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

/// Sync peer by URL (solves ID mismatch between Hub and Backend)
pub async fn sync_peer_by_url(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let db = state.db().clone();

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // 1. Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            // Peer not found locally, try to fetch from Hub
            let mut found_peer = None;

            if let Ok(hub_url) = std::env::var("HUB_URL") {
                let client = get_safe_client();
                let url = format!("{}/api/peers", hub_url);

                if let Ok(res) = client.get(&url).send().await
                    && res.status().is_success()
                {
                    #[derive(Deserialize)]
                    struct HubPeer {
                        name: String,
                        url: String,
                        #[serde(rename = "status")]
                        _status: String,
                    }
                    #[derive(Deserialize)]
                    struct HubResponse {
                        data: Vec<HubPeer>,
                    }

                    if let Ok(hub_res) = res.json::<HubResponse>().await {
                        for hub_peer in hub_res.data {
                            let hub_docker_url = translate_url_for_docker(&hub_peer.url);

                            // Match by URL
                            if hub_docker_url == docker_url {
                                // Insert new peer
                                let new_peer = peer::ActiveModel {
                                    name: Set(hub_peer.name),
                                    url: Set(hub_docker_url.clone()),
                                    created_at: Set(chrono::Utc::now().to_rfc3339()),
                                    updated_at: Set(chrono::Utc::now().to_rfc3339()),
                                    ..Default::default()
                                };

                                if let Ok(res) = peer::Entity::insert(new_peer).exec(&db).await {
                                    // Fetch the inserted peer to return it
                                    found_peer = peer::Entity::find_by_id(res.last_insert_id)
                                        .one(&db)
                                        .await
                                        .unwrap_or(None);
                                }
                                break;
                            }
                        }
                    }
                }
            }

            match found_peer {
                Some(p) => p,
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(
                            json!({ "error": format!("Peer not found with URL: {}", docker_url) }),
                        ),
                    )
                        .into_response();
                }
            }
        }
    };

    // 2. Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // 3. Validate URL
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();

    // 4. Check peer's config for privacy consent flags.
    // Skip the network fetch + port scan if the peer is cached as unreachable --
    // avoids burning 5-10s on timeouts we already know will fail.
    let peer_cached_unreachable = state.is_peer_direct_unreachable(peer.id);

    let mut peer_config = if peer_cached_unreachable {
        tracing::debug!(
            "Sync: Skipping config fetch for peer {} (cached unreachable)",
            peer.name,
        );
        None
    } else {
        let config_url = format!("{}/api/config", peer.url);
        match client.get(&config_url).send().await {
            Ok(res) if res.status().is_success() => {
                res.json::<crate::api::setup::ConfigResponse>().await.ok()
            }
            _ => None,
        }
    };

    // 4b. If config fetch failed, the peer may have restarted on a different port.
    // Try scanning ports 8000-8010 on the same host.
    // (Skip when peer is cached unreachable -- port scan would also timeout.)
    let effective_url = if peer_config.is_none() && !peer_cached_unreachable {
        match crate::utils::peer_discovery::try_discover_peer_port(&peer.url, &client).await {
            Some(new_url) => {
                // Retry config fetch with discovered URL
                let retry_url = format!("{}/api/config", new_url);
                peer_config = match client.get(&retry_url).send().await {
                    Ok(res) if res.status().is_success() => {
                        res.json::<crate::api::setup::ConfigResponse>().await.ok()
                    }
                    _ => None,
                };
                new_url
            }
            None => peer.url.clone(),
        }
    } else {
        peer.url.clone()
    };

    // Distinguish "peer explicitly disallows caching" from "peer unreachable"
    // When peer_config is None (unreachable on 5G), preserve cache and try E2EE/relay
    let peer_reachable = peer_config.is_some();
    let allows_caching = peer_config
        .as_ref()
        .map(|c| c.allow_library_caching)
        .unwrap_or(true); // assume caching OK when unreachable - preserve cache
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game_url = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle_url = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));
    let peer_display_name_url = peer_config
        .as_ref()
        .map(|c| c.library_name.clone())
        .unwrap_or_else(|| peer.name.clone());

    // Extract updated name from peer config (if changed)
    let updated_name = peer_config
        .as_ref()
        .filter(|c| c.library_name != peer.name)
        .map(|c| c.library_name.clone());

    // Extract updated avatar config from peer config (if changed)
    let updated_avatar = peer_config.as_ref().and_then(|c| {
        let new_json = c
            .avatar_config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        tracing::info!(
            "Sync avatar check for peer {}: remote={:?}, stored={:?}",
            peer.name,
            new_json.as_deref().map(|s| &s[..s.len().min(50)]),
            peer.avatar_config.as_deref().map(|s| &s[..s.len().min(50)]),
        );
        if new_json != peer.avatar_config {
            new_json
        } else {
            None
        }
    });

    // Refresh relay credentials from peer config if they changed
    if let Some(ref config) = peer_config {
        let new_relay = (
            config.relay_url.as_deref(),
            config.mailbox_id.as_deref(),
            config.relay_write_token.as_deref(),
        );
        let old_relay = (
            peer.relay_url.as_deref(),
            peer.mailbox_id.as_deref(),
            peer.relay_write_token.as_deref(),
        );
        if new_relay != old_relay
            && let (Some(r_url), Some(m_id), Some(w_tok)) = new_relay
            && !r_url.is_empty()
            && !m_id.is_empty()
            && !w_tok.is_empty()
            && let Ok(Some(existing)) = peer::Entity::find_by_id(peer.id).one(&db).await
        {
            let mut active: peer::ActiveModel = existing.into();
            active.relay_url = Set(Some(r_url.to_string()));
            active.mailbox_id = Set(Some(m_id.to_string()));
            active.relay_write_token = Set(Some(w_tok.to_string()));
            // ADR-032: fresh credentials from the peer's /api/config clear
            // any stale-token gate.
            active.relay_write_token_invalid_at = Set(None);
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(&db).await;
            tracing::info!(
                "Sync: Updated relay credentials for peer {} (mailbox: {})",
                peer.name,
                m_id
            );
        }
    }

    if peer_reachable && !allows_caching {
        // Peer is reachable and explicitly disallows caching - clear cache
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer.id))
            .exec(&db)
            .await;
        // Peer doesn't allow caching - still sync gamification stats
        sync_peer_gamification_stats(&db, peer.id, &effective_url, &client, shares_gamification)
            .await;
        // Still sync memory game scores
        crate::modules::memory_game::handlers::sync_peer_memory_scores(
            &db,
            peer.id,
            &effective_url,
            &peer_display_name_url,
            &client,
            peer_has_memory_game_url,
        )
        .await;
        // Still sync sliding puzzle scores
        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
            &db,
            peer.id,
            &effective_url,
            &peer_display_name_url,
            &client,
            peer_has_sliding_puzzle_url,
        )
        .await;

        let peer_id = peer.id;
        let url_changed = effective_url != peer.url;
        // Re-read peer from DB to avoid overwriting concurrent changes
        if let Ok(Some(fresh_peer)) = peer::Entity::find_by_id(peer_id).one(&db).await {
            let mut active_peer: peer::ActiveModel = fresh_peer.into();
            if url_changed {
                active_peer.url = Set(effective_url);
                tracing::info!("Port discovery: persisted new URL for peer {}", peer_id);
            }
            if let Some(ref new_name) = updated_name {
                active_peer.name = Set(new_name.clone());
                tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
            }
            if let Some(ref avatar) = updated_avatar {
                active_peer.avatar_config = Set(Some(avatar.clone()));
            }
            active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active_peer.update(&db).await;
        }

        return (
            StatusCode::OK,
            Json(json!({
                "message": "Peer does not allow library caching",
                "count": 0,
                "peer_id": peer_id,
                "caching_allowed": false
            })),
        )
            .into_response();
    }

    // 4. Fetch remote books — try E2EE first, then plaintext fallback
    // When the peer is unreachable via direct HTTP (e.g. on 5G), avatar_config and
    // library_name are also piggy-backed on the E2EE response as a fallback.
    //
    // Diff-based: send the catalog_hash we cached on the peer row last time.
    // The responder (handle_book_sync_request) returns a tiny "unchanged"
    // payload when its current hash matches, saving the ~95 KB book list on
    // every uneventful poll.
    let mut e2ee_avatar: Option<String> = None;
    let mut e2ee_library_name: Option<String> = None;
    let mut sync_unchanged: bool = false;
    let mut new_catalog_hash: Option<String> = None;
    let cached_catalog_hash = peer.catalog_hash.clone();
    let request_payload = match cached_catalog_hash.as_deref() {
        Some(h) => json!({ "catalog_hash": h }),
        None => json!({}),
    };
    let books: Vec<crate::models::Book> =
        match try_send_e2ee(&state, &peer, "book_sync_request", request_payload).await {
            Ok(Some(Some(response_msg))) => {
                // Extract avatar and library name for relay-only sync (5G fallback)
                e2ee_avatar = response_msg
                    .payload
                    .get("avatar_config")
                    .and_then(|v| serde_json::to_string(v).ok())
                    .filter(|s| s != "null" && !s.is_empty());
                e2ee_library_name = response_msg
                    .payload
                    .get("library_name")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));
                new_catalog_hash = response_msg
                    .payload
                    .get("catalog_hash")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));

                let status = response_msg.payload.get("status").and_then(|v| v.as_str());
                if status == Some("unchanged") {
                    // Catalog unchanged: keep the previous local cache
                    // entries intact and skip the (now redundant) upsert.
                    sync_unchanged = true;
                    Vec::new()
                } else {
                    // Got encrypted book list
                    serde_json::from_value(
                        response_msg
                            .payload
                            .get("books")
                            .cloned()
                            .unwrap_or(json!([])),
                    )
                    .unwrap_or_default()
                }
            }
            Ok(Some(None)) => {
                // E2EE sent but no response body (unexpected for sync)
                vec![]
            }
            Ok(None) | Err(_) => {
                // Fallback to plaintext
                let url = format!("{}/api/books?owned_only=true", effective_url);
                match client.get(&url).send().await {
                    Ok(response) if response.status().is_success() => {
                        #[derive(Deserialize)]
                        struct BooksResponse {
                            books: Vec<crate::models::Book>,
                        }
                        response
                            .json::<BooksResponse>()
                            .await
                            .map(|d| d.books)
                            .unwrap_or_default()
                    }
                    Ok(_) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": "Peer returned error" })),
                        )
                            .into_response();
                    }
                    Err(_) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": "Failed to contact peer" })),
                        )
                            .into_response();
                    }
                }
            }
        };

    // 5. Upsert books cache (preserves first_seen_at).
    // Skip when the responder reported "unchanged": the cache already has
    // the correct entries from the previous successful sync, and re-running
    // the upsert with an empty `books` would mistakenly delete them.
    let count = if sync_unchanged {
        crate::models::peer_book::Entity::find()
            .filter(crate::models::peer_book::Column::PeerId.eq(peer.id))
            .count(&db)
            .await
            .unwrap_or(0) as usize
    } else {
        // The E2EE book_sync response carries the peer's whole catalog in one
        // payload (or "unchanged", handled above), so this is a full snapshot.
        upsert_peer_books_cache(&db, peer.id, None, books, true).await
    };

    // 6. Sync gamification stats
    sync_peer_gamification_stats(&db, peer.id, &effective_url, &client, shares_gamification).await;

    // 6b. Sync memory game scores
    crate::modules::memory_game::handlers::sync_peer_memory_scores(
        &db,
        peer.id,
        &effective_url,
        &peer_display_name_url,
        &client,
        peer_has_memory_game_url,
    )
    .await;

    // 6c. Sync sliding puzzle scores
    crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
        &db,
        peer.id,
        &effective_url,
        &peer_display_name_url,
        &client,
        peer_has_sliding_puzzle_url,
    )
    .await;

    // 7. Update peer's last_seen (and name/URL/avatar if changed)
    // Re-read the peer from DB to avoid overwriting concurrent changes
    // (the `peer` variable was loaded at the start of this function and is stale).
    let peer_id = peer.id;
    let url_changed = effective_url != peer.url;
    // Fall back to E2EE-sourced metadata when peer was unreachable via direct HTTP (5G)
    let final_updated_name = updated_name.or_else(|| e2ee_library_name.filter(|n| n != &peer.name));
    let final_updated_avatar = updated_avatar
        .or_else(|| e2ee_avatar.filter(|a| Some(a.as_str()) != peer.avatar_config.as_deref()));
    if let Ok(Some(fresh_peer)) = peer::Entity::find_by_id(peer_id).one(&db).await {
        let mut active_peer: peer::ActiveModel = fresh_peer.into();
        if url_changed {
            active_peer.url = Set(effective_url);
            tracing::info!("Port discovery: persisted new URL for peer {}", peer_id);
        }
        if let Some(ref new_name) = final_updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        if let Some(ref avatar) = final_updated_avatar {
            active_peer.avatar_config = Set(Some(avatar.clone()));
        }
        // Persist the catalog hash returned by the responder so the next
        // book_sync_request can short-circuit to "unchanged" when the peer
        // catalog is still the same. Only update when we actually got a
        // hash back (avoid clobbering with None on transport errors).
        if let Some(ref hash) = new_catalog_hash {
            active_peer.catalog_hash = Set(Some(hash.clone()));
            active_peer.last_catalog_sync = Set(Some(chrono::Utc::now().to_rfc3339()));
        }
        active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active_peer.update(&db).await;
    }

    (
        StatusCode::OK,
        Json(json!({ "message": "Sync successful", "count": count, "peer_id": peer_id })),
    )
        .into_response()
}

#[cfg(test)]
mod added_at_tests {
    use super::*;
    use crate::db;
    use crate::models::{peer, peer_book};
    use sea_orm::Set;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer(db: &DatabaseConnection) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let p = peer::ActiveModel {
            name: Set("test-peer".to_string()),
            url: Set("http://test-peer.local:8080".to_string()),
            last_seen: Set(Some(now.clone())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        peer::Entity::insert(p)
            .exec(db)
            .await
            .unwrap()
            .last_insert_id
    }

    async fn insert_peer_book(
        db: &DatabaseConnection,
        peer_id: i32,
        remote_book_id: &str,
        title: &str,
        added_at: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        let pb = peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set(remote_book_id.to_string()),
            title: Set(title.to_string()),
            isbn: Set(None),
            author: Set(None),
            cover_url: Set(None),
            summary: Set(None),
            synced_at: Set(now),
            node_id: Set(None),
            first_seen_at: Set(None),
            added_at: Set(added_at.map(|s| s.to_string())),
            notified_at: Set(None),
            owned: Set(true),
            available_copies: Set(None),
            ..Default::default()
        };
        peer_book::Entity::insert(pb).exec(db).await.unwrap();
    }

    /// peer_book::Model → Book mapping must put remote_book_id into Book.id
    /// and propagate `added_at` (the owner's `books.created_at`) so cached
    /// and live responses agree on both id space and the "new" badge.
    #[tokio::test]
    async fn from_peer_book_uses_remote_id_and_added_at() {
        let pb = peer_book::Model {
            id: 999, // local row PK — must NOT be exposed as Book.id
            peer_id: 1,
            remote_book_id: "42".to_string(),
            title: "Le Livre".to_string(),
            isbn: Some("978".to_string()),
            author: Some("X".to_string()),
            cover_url: None,
            summary: None,
            synced_at: "2026-04-13T00:00:00Z".to_string(),
            node_id: None,
            first_seen_at: None,
            added_at: Some("2026-04-13T08:00:00Z".to_string()),
            notified_at: None,
            owned: true,
            available_copies: Some(2),
        };
        let book: crate::models::Book = pb.into();
        assert_eq!(
            book.id,
            Some("42".to_string()),
            "Book.id must be remote_book_id, not peer_book.id"
        );
        assert_eq!(book.added_at.as_deref(), Some("2026-04-13T08:00:00Z"));
        assert_eq!(book.title, "Le Livre");
        assert_eq!(book.owned, Some(true));
        assert_eq!(book.available_copies, Some(2));
    }

    /// Loan status from the owner (owned=false, available_copies=Some(0))
    /// must round-trip through the cache so the peer-lib carousel can hide
    /// non-requestable books without re-querying the peer.
    #[tokio::test]
    async fn upsert_peer_books_cache_persists_loan_status() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        let books = vec![
            crate::models::Book {
                id: Some("10".to_string()),
                title: "Borrowed by peer".to_string(),
                owned: Some(false),
                available_copies: Some(1),
                ..Default::default()
            },
            crate::models::Book {
                id: Some("11".to_string()),
                title: "All copies on loan".to_string(),
                owned: Some(true),
                available_copies: Some(0),
                ..Default::default()
            },
            crate::models::Book {
                id: Some("12".to_string()),
                title: "Available".to_string(),
                owned: Some(true),
                available_copies: Some(2),
                ..Default::default()
            },
        ];
        upsert_peer_books_cache(&db, peer_id, None, books, true).await;

        let fetch = |remote_id: String| {
            let db = db.clone();
            async move {
                peer_book::Entity::find()
                    .filter(peer_book::Column::PeerId.eq(peer_id))
                    .filter(peer_book::Column::RemoteBookId.eq(remote_id))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let borrowed = fetch("10".to_string()).await;
        let fully_lent = fetch("11".to_string()).await;
        let available = fetch("12".to_string()).await;

        assert!(
            !borrowed.owned,
            "peer-borrowed book must persist owned=false"
        );
        assert_eq!(borrowed.available_copies, Some(1));
        assert!(fully_lent.owned);
        assert_eq!(
            fully_lent.available_copies,
            Some(0),
            "available_copies=0 must round-trip so the carousel filter can drop fully-lent books",
        );
        assert!(available.owned);
        assert_eq!(available.available_copies, Some(2));

        // UPDATE path: a later sync marks the available book as fully lent.
        let updated = vec![crate::models::Book {
            id: Some("12".to_string()),
            title: "Available".to_string(),
            owned: Some(true),
            available_copies: Some(0),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, updated, true).await;
        let refreshed = fetch("12".to_string()).await;
        assert_eq!(
            refreshed.available_copies,
            Some(0),
            "update must refresh available_copies to reflect the current loan state",
        );
    }

    /// A PARTIAL fetch (`is_full_snapshot = false`) must be additive: books
    /// already cached but absent from the partial batch MUST survive. This is
    /// the cache-drain bug guard — a paginated first page or a relay loop cut
    /// short by a timeout must never wipe the rest of the catalog.
    #[tokio::test]
    async fn upsert_peer_books_cache_partial_fetch_keeps_absent_books() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        // Cache three books from a previous full sync.
        insert_peer_book(&db, peer_id, "1", "Alpha", None).await;
        insert_peer_book(&db, peer_id, "2", "Beta", None).await;
        insert_peer_book(&db, peer_id, "3", "Gamma", None).await;

        // A partial batch arrives carrying only the first book.
        let partial = vec![crate::models::Book {
            id: Some("1".to_string()),
            title: "Alpha".to_string(),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, partial, false).await;

        let remaining = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            remaining, 3,
            "partial fetch must not delete books absent from the batch",
        );
    }

    /// A FULL snapshot (`is_full_snapshot = true`) IS authoritative: books
    /// the owner removed (absent from the complete batch) must be pruned.
    #[tokio::test]
    async fn upsert_peer_books_cache_full_snapshot_prunes_absent_books() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        insert_peer_book(&db, peer_id, "1", "Alpha", None).await;
        insert_peer_book(&db, peer_id, "2", "Beta", None).await;
        insert_peer_book(&db, peer_id, "3", "Gamma", None).await;

        // Full catalog: the owner now only has books 1 and 3 (book 2 deleted).
        let snapshot = vec![
            crate::models::Book {
                id: Some("1".to_string()),
                title: "Alpha".to_string(),
                ..Default::default()
            },
            crate::models::Book {
                id: Some("3".to_string()),
                title: "Gamma".to_string(),
                ..Default::default()
            },
        ];
        upsert_peer_books_cache(&db, peer_id, None, snapshot, true).await;

        let ids: Vec<String> = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.remote_book_id)
            .collect();
        assert_eq!(ids.len(), 2, "full snapshot must prune the removed book");
        assert!(
            !ids.contains(&"2".to_string()),
            "book 2 was deleted by the owner"
        );
    }

    /// `redact_for_peer` strips personal fields but MUST keep `added_at`:
    /// it is editorial metadata (the owner's `books.created_at`) that
    /// drives the "new" badge for every viewer.
    #[test]
    fn redact_for_peer_preserves_added_at() {
        let mut book = crate::models::Book {
            id: Some("1".to_string()),
            title: "T".to_string(),
            user_rating: Some(8),
            reading_status: Some("read".to_string()),
            added_at: Some("2026-04-15T10:00:00+00:00".to_string()),
            ..Default::default()
        };
        book.redact_for_peer();
        assert_eq!(book.user_rating, None, "rating must be redacted");
        assert_eq!(book.reading_status, None, "reading status must be redacted");
        assert_eq!(
            book.added_at.as_deref(),
            Some("2026-04-15T10:00:00+00:00"),
            "added_at is editorial metadata, not personal — must NOT be redacted",
        );
    }

    /// Upserting a previously-cached book with a fresh `added_at` from the
    /// owner must overwrite the local value (owner is source of truth).
    #[tokio::test]
    async fn upsert_peer_books_cache_refreshes_added_at() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;
        insert_peer_book(
            &db,
            peer_id,
            "7",
            "Cached",
            Some("2026-01-01T00:00:00+00:00"),
        )
        .await;

        let books = vec![crate::models::Book {
            id: Some("7".to_string()),
            title: "Cached".to_string(),
            added_at: Some("2026-04-15T12:00:00+00:00".to_string()),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, books, true).await;

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq("7"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-04-15T12:00:00+00:00"),
            "owner's added_at must overwrite the local value on upsert",
        );
    }

    /// Inserting a brand-new book via the cache upsert must persist its
    /// `added_at` as broadcast by the owner (not derived from local time).
    #[tokio::test]
    async fn upsert_peer_books_cache_persists_added_at_on_insert() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        let books = vec![crate::models::Book {
            id: Some("99".to_string()),
            title: "New".to_string(),
            added_at: Some("2026-04-15T09:30:00+00:00".to_string()),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, books, true).await;

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq("99"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-04-15T09:30:00+00:00"),
            "owner's added_at must be persisted on first insert",
        );
    }

    /// wishlist_match must fire only for books the peer actually OWNS.
    /// An entry the peer merely wishes for (owned=false, e.g. delivered by
    /// the delta path, which does not filter non-owned books) is not
    /// borrowable and must not notify. Once the peer acquires the book
    /// (owned flips to true on a later sync), the notification must fire.
    #[tokio::test]
    async fn upsert_peer_books_cache_wishlist_match_requires_owned() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        // Local wishlist entry with a matching ISBN
        let now = chrono::Utc::now().to_rfc3339();
        crate::models::book::ActiveModel {
            title: Set("La Condition humaine".to_string()),
            isbn: Set(Some("9782070360208".to_string())),
            reading_status: Set("wanting".to_string()),
            owned: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let wishlist_matches = |db: DatabaseConnection| async move {
            crate::models::notification::Entity::find()
                .filter(crate::models::notification::Column::EventType.eq("wishlist_match"))
                .all(&db)
                .await
                .unwrap()
        };

        // The peer's sync delivers the same ISBN, but the peer only wishes it too
        let books = vec![crate::models::Book {
            id: Some("20".to_string()),
            title: "La Condition humaine".to_string(),
            isbn: Some("9782070360208".to_string()),
            owned: Some(false),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, books, true).await;

        assert!(
            wishlist_matches(db.clone()).await.is_empty(),
            "a book in the peer's own wishlist (owned=false) must not trigger wishlist_match",
        );

        // The non-owned row must stay un-notified so a later acquisition can fire
        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq("20"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            row.notified_at.is_none(),
            "non-owned entries must stay un-notified until the peer acquires the book",
        );

        // The peer acquires the book: the next sync flips owned to true
        let books = vec![crate::models::Book {
            id: Some("20".to_string()),
            title: "La Condition humaine".to_string(),
            isbn: Some("9782070360208".to_string()),
            owned: Some(true),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, books, true).await;

        assert_eq!(
            wishlist_matches(db.clone()).await.len(),
            1,
            "wishlist_match must fire once the peer actually owns the book",
        );
    }
}
