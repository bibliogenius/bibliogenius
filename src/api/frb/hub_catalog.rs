// Hub catalog push/sync/fetch and the directory catalog cache.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

/// Pushes the local ISBN list to the hub catalog cache (legacy, ISBN-only).
pub async fn hub_directory_push_catalog(isbn_list: Vec<String>) -> Result<(), String> {
    use crate::services::hub_directory_service::CatalogEntry;
    let db = hub_db()?;
    let book_count = crate::services::book_service::count_books(db)
        .await
        .map_err(|e| format!("count_books: {e:?}"))?;
    let entries: Vec<CatalogEntry> = isbn_list
        .into_iter()
        .map(|isbn| CatalogEntry {
            isbn,
            book_id: None,
            title: String::new(),
            author: None,
            cover_url: None,
            added_at: None,
        })
        .collect();
    hub_directory_svc()
        .push_catalog(db, &entries, book_count)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Reads all owned books from the local database, collects title, author,
/// and cover data, and pushes the enriched catalog to the hub.
/// Every entry carries its local `book_id` so the hub-side cover GC
/// (ADR-033) can diff the catalog against the `covers/{node}/{id}.jpg`
/// files on disk. Books without ISBN are still included; the entry is
/// keyed by `book_id` alone in that case.
/// Local cover images are resized and uploaded as thumbnails (best-effort).
/// Returns the number of entries pushed.
pub async fn hub_directory_sync_catalog() -> Result<i32, String> {
    let result = hub_directory_sync_catalog_inner().await;
    if let Err(ref e) = result {
        // Best-effort diagnostic beacon: a sync that fails before/at the push
        // is otherwise invisible server-side (the POST never lands). Report the
        // failure into the hub's hub_events table so it surfaces in DB backups
        // without needing the device log. Never alters the returned result.
        if let Ok(db) = hub_db() {
            hub_directory_svc()
                .report_sync_diag(db, "sync_catalog", false, e)
                .await;
        }
    }
    result
}

/// Inner worker for [`hub_directory_sync_catalog`]: builds the enriched
/// catalog and pushes it to the hub. Kept separate so the public entry point
/// can beacon any failure for diagnosis without changing the FFI contract.
async fn hub_directory_sync_catalog_inner() -> Result<i32, String> {
    use crate::models::book::{Column as BookColumn, Entity as BookEntity};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = hub_db()?;

    // Verify the library is registered before doing any work.
    let _cfg = crate::services::hub_directory_service::HubDirectoryService::get_config(db)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Not registered in directory".to_string())?;

    // Collect ALL owned books with their authors (no ISBN filter).
    let books_with_authors: Vec<(
        crate::models::book::Model,
        Vec<crate::models::author::Model>,
    )> = BookEntity::find()
        .filter(BookColumn::Owned.eq(true))
        .find_with_related(crate::models::author::Entity)
        .all(db)
        .await
        .map_err(|e| format!("DB error: {e}"))?;

    let svc = hub_directory_svc();

    let mut entries: Vec<CatalogEntry> = Vec::new();
    // Map book_id -> entry index for updating cover URLs after upload
    let mut id_to_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    // (book_id, local_cover_path, updated_at) - updated_at is needed at
    // upload-completion time to append the ?v=tag cache-buster so peers
    // refetch immediately after a re-upload.
    let mut local_covers: Vec<(String, String, String)> = Vec::new();

    for (book, authors) in books_with_authors {
        let isbn = book
            .isbn
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let book_id_val = book.id;
        let book_updated_at = book.updated_at.clone();
        // Always include book_id: the hub's cover GC (ADR-033) uses it to
        // diff catalog entries against `covers/{node}/{book_id}.jpg` files
        // on disk. Omitting it on ISBN-bearing entries silently disables GC
        // and leaks orphan covers (see `skipped_empty_catalog` warnings).
        let book_id = Some(book_id_val.clone());

        // Skip books with neither ISBN nor title (unusable entries)
        if isbn.is_empty() && book.title.is_empty() {
            continue;
        }

        let author = if authors.is_empty() {
            None
        } else {
            Some(
                authors
                    .into_iter()
                    .map(|a| a.name)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };

        // S5: only HTTP/HTTPS cover URLs go to the hub catalog directly.
        // Local file paths are collected for thumbnail upload.
        let cover_url_raw = book.cover_url.unwrap_or_default();
        let cover_url = if crate::utils::cover_url::is_servable_remotely(&cover_url_raw) {
            Some(cover_url_raw)
        } else if !cover_url_raw.is_empty() {
            // Local file path: schedule for thumbnail upload
            local_covers.push((book_id_val.clone(), cover_url_raw, book_updated_at));
            None // Will be updated after upload
        } else {
            None
        };

        let idx = entries.len();
        // book.created_at is the owner's authoritative "added to library"
        // timestamp. Carrying it on the catalog entry lets every follower
        // agree on whether a book is recent (source of truth for the
        // "NEW" badge on the viewer side), instead of relying on the
        // per-device first_seen_at heuristic.
        entries.push(CatalogEntry {
            isbn,
            book_id,
            title: book.title,
            author,
            cover_url,
            added_at: Some(book.created_at),
        });
        id_to_index.insert(book_id_val, idx);
    }

    // Upload local cover thumbnails to the hub. A failure here leaves
    // `entries[idx].cover_url = None`, so the peer sees no cover for this
    // book until the next sync retries (naturally: the next sync re-iterates
    // all owned books and re-attempts the upload, the new catalog payload
    // includes the now-filled cover_url so its hash differs and the push
    // goes through). Logged at ERROR so the failure is diagnosable rather
    // than drowned in warn-level noise.
    for (bid, path, updated_at) in &local_covers {
        if let Some(hub_url) = svc.process_local_cover_upload(db, bid, path).await
            && let Some(&idx) = id_to_index.get(bid)
        {
            // Append the canonical ?v=tag so peers bust their
            // CachedNetworkImage cache when the owner re-uploads.
            let versioned =
                crate::models::Book::append_cover_version_tag(hub_url, Some(updated_at.as_str()));
            entries[idx].cover_url = Some(versioned);
        }
    }

    let count = entries.len() as i32;
    // Hub-profile book_count matches what followers actually see. Using
    // `entries.len()` (owned + isbn-or-title) instead of a raw `books` row
    // count avoids inflating the public number with wishlist rows, stale
    // sync entries, or owned books that were filtered out of the catalog.
    let book_count = count as i64;

    // Always push: even with an empty catalog, book_count must reach the hub.
    // push_catalog short-circuits when the catalog hasn't changed (ADR-027);
    // we log the outcome but keep returning the entry count so the Flutter
    // provider clears its `_catalogDirty` flag either way.
    let outcome = svc
        .push_catalog(db, &entries, book_count)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(
        target: "hub_directory",
        outcome = ?outcome,
        count = count,
        "hub catalog sync outcome"
    );

    Ok(count)
}

/// Browses the hub public directory.
///
/// ADR-035 Phase 2: `city_id` filters by GeoNames id (combinable with
/// `country` and `search`). Pass `None` to return all listed libraries.
pub async fn hub_directory_list(
    limit: i64,
    offset: i64,
    country: Option<String>,
    search: Option<String>,
    city_id: Option<i64>,
) -> Result<Vec<FrbHubProfile>, String> {
    hub_directory_svc()
        .list_directory(
            limit,
            offset,
            country.as_deref(),
            search.as_deref(),
            city_id,
        )
        .await
        .map(|v| v.into_iter().map(FrbHubProfile::from).collect())
        .map_err(|e| e.to_string())
}

/// Gets a specific library profile from the hub directory.
pub async fn hub_directory_get_profile(node_id: String) -> Result<FrbHubProfile, String> {
    let db = hub_db()?;
    hub_directory_svc()
        .get_profile(db, &node_id)
        .await
        .map(FrbHubProfile::from)
        .map_err(|e| e.to_string())
}

/// Detailed outcome of a hub catalog fetch, so the UI can distinguish an
/// empty catalog from a denied, expired, or unreachable one instead of
/// rendering every failure as "no books".
pub struct FrbHubCatalogResult {
    pub entries: Vec<FrbCatalogEntry>,
    /// "hub" when entries come from a live hub response, "cache" when the
    /// hub fetch failed and entries are the local offline cache.
    pub source: String,
    /// Machine-readable failure reason, set only when `source == "cache"`:
    /// - "follow_required": the library gates its catalog behind an
    ///   approved follow (hub 403 with code)
    /// - "catalog_unavailable": access is fine but the hub holds no
    ///   catalog (expired TTL or never pushed)
    /// - "not_found": no hub profile for this node id
    /// - "http_<status>": other hub error without a machine code
    /// - "network": transport failure or local config issue
    pub error_code: Option<String>,
}

/// Maps a hub directory error to the machine-readable codes documented on
/// [FrbHubCatalogResult::error_code]. Prefers the `code` field the hub
/// attaches to error bodies; falls back to the HTTP status for older hubs.
fn hub_catalog_error_code(e: &HubDirectoryError) -> String {
    match e {
        HubDirectoryError::Hub(status, body) => serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_string))
            .unwrap_or_else(|| format!("http_{status}")),
        _ => "network".to_string(),
    }
}

/// Gets the catalog of a library (public or approved follow).
/// Fetches from hub, upserts into local cache, and returns entries with added_at.
/// If the hub fetch fails, returns the cached entries (offline-first) along
/// with an honest error code describing why the hub could not serve.
pub async fn hub_directory_get_catalog_detailed(
    node_id: String,
) -> Result<FrbHubCatalogResult, String> {
    use crate::models::peer_book;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = hub_db()?;

    // Try to fetch fresh catalog from hub
    let hub_result = hub_directory_svc().get_catalog(db, &node_id).await;

    match hub_result {
        Ok(entries) => {
            tracing::debug!(
                "hub_directory_get_catalog: fetched {} entries, upserting cache",
                entries.len()
            );
            // Upsert into local cache and return with owner-side added_at
            let result = upsert_directory_catalog_cache(db, &node_id, &entries).await;
            Ok(FrbHubCatalogResult {
                entries: result,
                source: "hub".to_string(),
                error_code: None,
            })
        }
        Err(ref e) => {
            let error_code = hub_catalog_error_code(e);
            tracing::warn!(
                "hub_directory_get_catalog: hub fetch failed ({}, code={}), using cache",
                e,
                error_code
            );
            // Offline fallback: return cached entries
            let cached = peer_book::Entity::find()
                .filter(peer_book::Column::NodeId.eq(&node_id))
                .filter(peer_book::Column::PeerId.eq(0))
                .all(db)
                .await
                .unwrap_or_default();

            Ok(FrbHubCatalogResult {
                entries: cached
                    .into_iter()
                    .filter_map(|pb| {
                        pb.isbn.map(|isbn| FrbCatalogEntry {
                            isbn,
                            title: pb.title,
                            author: pb.author,
                            cover_url: pb.cover_url,
                            // Offline: trust the last `added_at` we received from the
                            // owner. Legacy cached rows (pre-added_at push) have None
                            // here, which correctly suppresses the "NEW" badge.
                            added_at: pb.added_at,
                        })
                    })
                    .collect(),
                source: "cache".to_string(),
                error_code: Some(error_code),
            })
        }
    }
}

/// Compatibility wrapper around [hub_directory_get_catalog_detailed] for
/// callers that only need the entries (directory browsing screens).
pub async fn hub_directory_get_catalog(node_id: String) -> Result<Vec<FrbCatalogEntry>, String> {
    hub_directory_get_catalog_detailed(node_id)
        .await
        .map(|r| r.entries)
}

#[cfg(test)]
mod hub_catalog_error_code_tests {
    use super::*;

    #[test]
    fn prefers_machine_code_from_hub_body() {
        let e = HubDirectoryError::Hub(
            403,
            r#"{"error":"Access requires an active follow relationship.","code":"follow_required"}"#
                .to_string(),
        );
        assert_eq!(hub_catalog_error_code(&e), "follow_required");
    }

    #[test]
    fn falls_back_to_http_status_for_older_hubs() {
        let e = HubDirectoryError::Hub(403, r#"{"error":"Catalog not available."}"#.to_string());
        assert_eq!(hub_catalog_error_code(&e), "http_403");
    }

    #[test]
    fn tolerates_non_json_bodies() {
        let e = HubDirectoryError::Hub(502, "Bad Gateway".to_string());
        assert_eq!(hub_catalog_error_code(&e), "http_502");
    }

    #[test]
    fn maps_transport_failures_to_network() {
        let e = HubDirectoryError::Network("connection refused".to_string());
        assert_eq!(hub_catalog_error_code(&e), "network");
    }
}

/// Additive merge of an incoming directory-catalog entry over the cached row.
/// An empty/None incoming field never erases cached knowledge: the hub
/// legitimately serves degraded (ISBN-only) entries when the owner runs an
/// older build (see `get_catalog`'s fallback), while a real metadata change
/// always carries a non-empty value. Erasing would be sticky — cache-only
/// loads would keep showing blank teasers even after the hub recovers.
fn merge_directory_entry(
    cached: &crate::models::peer_book::Model,
    entry: &CatalogEntry,
) -> CatalogEntry {
    CatalogEntry {
        isbn: entry.isbn.clone(),
        book_id: entry.book_id.clone(),
        title: if entry.title.is_empty() {
            cached.title.clone()
        } else {
            entry.title.clone()
        },
        author: entry.author.clone().or_else(|| cached.author.clone()),
        cover_url: entry.cover_url.clone().or_else(|| cached.cover_url.clone()),
        added_at: entry.added_at.clone().or_else(|| cached.added_at.clone()),
    }
}

/// Upserts directory catalog entries into peer_books cache (peer_id = 0 sentinel).
/// Returns entries enriched with the authoritative `added_at` from the owner
/// (carried on every CatalogEntry). `first_seen_at` is still populated for
/// legacy reasons (viewer-local timestamp) but is no longer used for the
/// "NEW" badge - `added_at` is the single source of truth now.
async fn upsert_directory_catalog_cache(
    db: &DatabaseConnection,
    node_id: &str,
    entries: &[CatalogEntry],
) -> Vec<FrbCatalogEntry> {
    use crate::models::peer_book;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

    let now = chrono::Utc::now().to_rfc3339();

    // Load existing cached entries for this directory library
    let existing = peer_book::Entity::find()
        .filter(peer_book::Column::NodeId.eq(node_id))
        .filter(peer_book::Column::PeerId.eq(0))
        .all(db)
        .await
        .unwrap_or_default();

    // Guard: an empty incoming catalog with a non-empty cache would wipe
    // every cached row in the prune pass below. The hub can legitimately
    // serve an empty catalog (a peer re-pushing right after a reinstall,
    // before importing its books), but this cache is the only offline
    // fallback for the library, so the destructive sync is skipped. Mirrors
    // the LAN guard in upsert_peer_books_cache.
    if entries.is_empty() && !existing.is_empty() {
        tracing::warn!(
            "upsert_directory_catalog_cache: node_id={} - incoming catalog empty but {} cached entries exist, skipping destructive prune",
            node_id,
            existing.len(),
        );
        return Vec::new();
    }

    // Index the cache by canonical ISBN key: valid ISBNs compare in their
    // ISBN-13 form (the same edition circulates as ISBN-10 on one side and
    // ISBN-13 on the other, so raw-string comparison would duplicate the
    // entry, miss the metadata update, and let the prune pass delete the row
    // stored under the other form), while unparseable values (empty or
    // malformed) keep their raw form unchanged and only ever match
    // themselves. The historical raw-form matching could cache the same book
    // twice, once per ISBN form; such duplicates collapse onto one key here: the shadowed
    // row's knowledge is folded additively into the kept row and the shadowed
    // row is deleted. The in-memory fold is never lost because every kept row
    // is persisted afterwards, either by the UPDATE branch (book still in the
    // catalog) or by the prune pass (book gone). Unparseable keys keep the
    // historical last-wins overwrite without deleting: two ISBN-less rows are
    // different books, folding them would destroy one.
    let mut existing_map: std::collections::HashMap<String, peer_book::Model> =
        std::collections::HashMap::new();
    let mut shadowed_duplicate_ids: Vec<i32> = Vec::new();
    for row in existing {
        let Some(raw_isbn) = row.isbn.clone() else {
            continue;
        };
        let canonical = crate::utils::isbn::to_isbn13(&raw_isbn);
        let key = canonical.clone().unwrap_or_else(|| raw_isbn.clone());
        match existing_map.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(row);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if canonical.is_some() {
                    let kept = slot.get_mut();
                    if kept.title.is_empty() {
                        kept.title = row.title.clone();
                    }
                    if kept.author.is_none() {
                        kept.author = row.author.clone();
                    }
                    if kept.cover_url.is_none() {
                        kept.cover_url = row.cover_url.clone();
                    }
                    if kept.added_at.is_none() {
                        kept.added_at = row.added_at.clone();
                    }
                    shadowed_duplicate_ids.push(row.id);
                } else {
                    slot.insert(row);
                }
            }
        }
    }
    for id in shadowed_duplicate_ids {
        let _ = peer_book::Entity::delete_by_id(id).exec(db).await;
    }

    let mut fresh_isbns = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(entries.len());
    // New sentinel rows (peer_id = 0) are inserted in a single FK-isolated
    // batch after the loop, not inline, so the foreign_keys=OFF window stays on
    // a dedicated connection (see the batch block below). Tuple holds the
    // per-entry values: (title, isbn, author, cover_url, added_at).
    #[allow(clippy::type_complexity)]
    let mut to_insert: Vec<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = Vec::new();

    for entry in entries {
        let canonical = crate::utils::isbn::to_isbn13(&entry.isbn);
        let entry_is_valid_isbn = canonical.is_some();
        let entry_key = canonical.unwrap_or_else(|| entry.isbn.clone());
        // A catalog listing the same book under both ISBN forms would
        // re-create the duplicate the fold above just cleaned, so only its
        // first occurrence is processed. Unparseable keys are exempt: two
        // ISBN-less entries are different books, not duplicates.
        if !fresh_isbns.insert(entry_key.clone()) && entry_is_valid_isbn {
            continue;
        }

        if let Some(existing_entry) = existing_map.get(&entry_key) {
            // UPDATE: additive refresh (see `merge_directory_entry`) + owner-side
            // added_at. `first_seen_at` stays untouched for any legacy reader
            // that still consults it.
            let merged = merge_directory_entry(existing_entry, entry);
            let mut active: peer_book::ActiveModel = existing_entry.clone().into();
            active.title = Set(merged.title.clone());
            active.author = Set(merged.author.clone());
            active.cover_url = Set(merged.cover_url.clone());
            active.added_at = Set(merged.added_at.clone());
            active.synced_at = Set(now.clone());
            let _ = active.update(db).await;

            result.push(FrbCatalogEntry {
                isbn: merged.isbn,
                title: merged.title,
                author: merged.author,
                cover_url: merged.cover_url,
                added_at: merged.added_at,
            });
        } else {
            // INSERT: collect for a single FK-isolated batch after the loop
            // (see the dedicated-connection block below). The sentinel row
            // carries peer_id = 0 (no matching peers row), summary = NULL,
            // synced_at = first_seen_at = now, notified_at = NULL (not yet
            // notified); owned/available_copies fall back to the column
            // defaults. added_at is the owner's broadcast timestamp (the "NEW"
            // badge source).
            to_insert.push((
                entry.title.clone(),
                entry.isbn.clone(),
                entry.author.clone(),
                entry.cover_url.clone(),
                entry.added_at.clone(),
            ));

            result.push(FrbCatalogEntry {
                isbn: entry.isbn.clone(),
                title: entry.title.clone(),
                author: entry.author.clone(),
                cover_url: entry.cover_url.clone(),
                added_at: entry.added_at.clone(),
            });
        }
    }

    // Insert the freshly-seen sentinel rows on a dedicated connection so the
    // foreign_keys=OFF window is isolated. Directory entries reference no real
    // peer, so the insert needs FK enforcement off; disabling it on a pooled
    // connection could leak (a later delete reusing that connection would skip
    // its ON DELETE CASCADE and orphan rows, the foreign-key cascade-orphan
    // root cause). A checked-out connection is invisible to concurrent operations
    // and is restored to ON before it returns to the pool.
    if !to_insert.is_empty() {
        match db.get_sqlite_connection_pool().acquire().await {
            Ok(mut conn) => {
                let _ = sqlx::query("PRAGMA foreign_keys = OFF")
                    .execute(&mut *conn)
                    .await;
                for (title, isbn, author, cover_url, added_at) in &to_insert {
                    if let Err(e) = sqlx::query(
                        "INSERT INTO peer_books \
                         (peer_id, remote_book_id, title, isbn, author, cover_url, \
                          summary, synced_at, node_id, first_seen_at, added_at, notified_at) \
                         VALUES (0, 0, ?, ?, ?, ?, NULL, ?, ?, ?, ?, NULL)",
                    )
                    .bind(title.as_str())
                    .bind(isbn.as_str())
                    .bind(author.as_deref())
                    .bind(cover_url.as_deref())
                    .bind(now.as_str())
                    .bind(node_id)
                    .bind(now.as_str())
                    .bind(added_at.as_deref())
                    .execute(&mut *conn)
                    .await
                    {
                        tracing::warn!("catalog cache insert failed for {isbn}: {e}");
                    }
                }
                // Restore enforcement before this connection rejoins the pool,
                // so no pooled connection ever lingers with FK disabled.
                let _ = sqlx::query("PRAGMA foreign_keys = ON")
                    .execute(&mut *conn)
                    .await;
            }
            Err(e) => {
                tracing::warn!("catalog cache: failed to acquire dedicated connection: {e}");
            }
        }
    }

    // Delete entries no longer in the catalog. Both sides compare in the
    // canonical key form (`existing_map` keys and `fresh_isbns` alike):
    // pruning on raw forms would delete a row still in the catalog but cached
    // under the other ISBN form.
    for (isbn_key, entry) in &existing_map {
        if !fresh_isbns.contains(isbn_key) {
            let _ = peer_book::Entity::delete_by_id(entry.id).exec(db).await;
        }
    }

    // Check un-notified entries for wishlist matches + emit "wishlist_match"
    // notification. Uses notified_at IS NULL instead of tracking inserts in
    // memory, so that notification dedup survives notification pruning (TTL/cap).
    // Only owned entries qualify (same rule as the peer-sync pass): a non-owned
    // entry is not borrowable and must not trigger a wishlist match.
    let unnotified = peer_book::Entity::find()
        .filter(peer_book::Column::NodeId.eq(node_id))
        .filter(peer_book::Column::PeerId.eq(0))
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

        if !new_isbns.is_empty() {
            // Resolve peer by library_uuid so we use the same ref_id as peer-sync
            // (avoids duplicate wishlist_match notifications from both paths)
            use crate::models::peer;
            let matching_peer = peer::Entity::find()
                .filter(peer::Column::LibraryUuid.eq(node_id))
                .one(db)
                .await
                .ok()
                .flatten();
            let display_name = matching_peer
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| node_id.to_string());
            let peer_ref_id = matching_peer
                .as_ref()
                .map(|p| p.id.to_string())
                .unwrap_or_else(|| format!("dir:{node_id}"));
            crate::services::notification_service::check_wishlist_matches(
                db,
                &new_isbns,
                &display_name,
                "peer",
                &peer_ref_id,
            )
            .await;
        }

        // Mark all un-notified entries as notified
        for pb in unnotified {
            let mut active: peer_book::ActiveModel = pb.into();
            active.notified_at = Set(Some(now.clone()));
            let _ = active.update(db).await;
        }
    }

    result
}
