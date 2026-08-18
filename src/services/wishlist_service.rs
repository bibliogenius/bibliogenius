//! Wishlist provider matching.
//!
//! Shared join between the local wishlist (books with reading_status =
//! 'wanting' and an ISBN) and the cached peer/directory catalogs
//! (peer_books). One join, several consumers:
//! - forward trigger: a peer/directory book enters the cache
//!   (notification_service::check_wishlist_matches);
//! - reverse trigger: a wish enters the library
//!   (book_service -> notify_providers_for_wish);
//! - availability displays: book details, wishlist filter, curated list
//!   import screen.
//!
//! Lane rule: `books.private = false` is enforced HERE, in this lane only.
//! A match leaks the wish outward (notification body names the source, and
//! the borrow button sends title + ISBN to the peer), so private books must
//! never match. Account sync deliberately does NOT share this filter: it
//! replicates private books between the user's own devices and must keep
//! doing so.
//!
//! Only `owned = true` cache rows qualify: a book the peer borrowed
//! themselves is not borrowable and must not match (same rule as the
//! forward pass in api/peer/sync.rs and api/frb/hub_catalog.rs).

use std::collections::{HashMap, HashSet};

use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter};

use crate::domain::notification_repository::{
    CreateNotification, NotificationEventType, NotificationRepository,
};
use crate::models::{book, peer, peer_book};

/// A cached peer/directory book that can fulfil a wishlist entry.
#[derive(Debug, Clone)]
pub struct WishlistProviderMatch {
    pub isbn: String,
    /// Local wanting book id. `None` when matching arbitrary ISBNs (e.g. a
    /// curated list previewed before import).
    pub book_id: Option<String>,
    /// Local wanting book title, when the match went through the books lane.
    pub book_title: Option<String>,
    /// Peer id owning the cache row; 0 = directory-only (hub catalog) entry.
    pub peer_id: i32,
    pub node_id: Option<String>,
    /// Peer display name, or the node id when the library is not paired.
    pub source_name: String,
    /// Paired peer URL, when known. Enables the P2P borrow path; directory
    /// entries without one go through the hub borrow request instead.
    pub peer_url: Option<String>,
    pub available_copies: Option<i32>,
    /// Notification ref prefix: "{peer_id}" or "dir:{node_id}". Matches the
    /// ref_id format of the forward pass (peer sync / hub catalog) so
    /// emit_unique dedups across both trigger directions.
    pub source_ref_id: String,
}

/// Borrow-eligible wishlist entries: wanting, with an ISBN, not private.
async fn wanting_books(db: &DatabaseConnection) -> Result<Vec<book::Model>, DbErr> {
    book::Entity::find()
        .filter(book::Column::ReadingStatus.eq("wanting"))
        .filter(book::Column::Isbn.is_not_null())
        .filter(book::Column::Private.eq(false))
        .all(db)
        .await
}

/// ISBNs of borrow-eligible wishlist entries. Used by the forward pass to
/// filter freshly cached peer books, so the private/wanting lane rule lives
/// in one place.
pub async fn wanting_isbn_set(db: &DatabaseConnection) -> Result<HashSet<String>, DbErr> {
    Ok(wanting_books(db)
        .await?
        .into_iter()
        .filter_map(|b| b.isbn)
        .collect())
}

/// Find owned cache rows matching the given ISBNs, with their source
/// resolved (peer name/URL when paired, node id otherwise).
///
/// A library both paired and followed yields two cache rows for the same
/// book (peer_id = X and peer_id = 0 + node_id); they are deduplicated on
/// (source, isbn) with the paired row taking precedence, mirroring how the
/// hub catalog pass reuses the peer ref_id for notifications.
pub async fn providers_for_isbns(
    db: &DatabaseConnection,
    isbns: &HashSet<String>,
) -> Result<Vec<WishlistProviderMatch>, DbErr> {
    if isbns.is_empty() {
        return Ok(Vec::new());
    }

    let rows = peer_book::Entity::find()
        .filter(peer_book::Column::Owned.eq(true))
        .filter(peer_book::Column::Isbn.is_in(isbns.iter().cloned()))
        .all(db)
        .await?;

    if rows.is_empty() {
        return Ok(Vec::new());
    }

    // Resolve sources: paired peers by id, followed libraries by library_uuid.
    let peer_ids: Vec<i32> = rows
        .iter()
        .map(|r| r.peer_id)
        .filter(|id| *id != 0)
        .collect();
    let node_ids: Vec<String> = rows.iter().filter_map(|r| r.node_id.clone()).collect();

    let mut peers_by_id: HashMap<i32, peer::Model> = HashMap::new();
    if !peer_ids.is_empty() {
        for p in peer::Entity::find()
            .filter(peer::Column::Id.is_in(peer_ids))
            .all(db)
            .await?
        {
            peers_by_id.insert(p.id, p);
        }
    }
    let mut peers_by_uuid: HashMap<String, peer::Model> = HashMap::new();
    if !node_ids.is_empty() {
        for p in peer::Entity::find()
            .filter(peer::Column::LibraryUuid.is_in(node_ids))
            .all(db)
            .await?
        {
            if let Some(uuid) = p.library_uuid.clone() {
                peers_by_uuid.insert(uuid, p);
            }
        }
    }

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut matches: Vec<WishlistProviderMatch> = Vec::new();

    // Paired-peer rows first so they win the (source, isbn) dedup below.
    let (peer_rows, directory_rows): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|r| r.peer_id != 0);

    for row in peer_rows.into_iter().chain(directory_rows) {
        let Some(isbn) = row.isbn.clone() else {
            continue;
        };
        let resolved_peer = if row.peer_id != 0 {
            peers_by_id.get(&row.peer_id)
        } else {
            row.node_id.as_ref().and_then(|n| peers_by_uuid.get(n))
        };
        let source_ref_id = match (resolved_peer, row.node_id.as_deref()) {
            (Some(p), _) => p.id.to_string(),
            (None, Some(node)) => format!("dir:{node}"),
            // Paired row whose peer record vanished: keep the raw id so the
            // ref stays stable, even if the name cannot be resolved.
            (None, None) => row.peer_id.to_string(),
        };
        if !seen.insert((source_ref_id.clone(), isbn.clone())) {
            continue;
        }
        let source_name = resolved_peer
            .map(|p| p.display_name.clone().unwrap_or_else(|| p.name.clone()))
            .or_else(|| row.node_id.clone())
            .unwrap_or_else(|| source_ref_id.clone());
        matches.push(WishlistProviderMatch {
            isbn,
            book_id: None,
            book_title: None,
            peer_id: resolved_peer.map(|p| p.id).unwrap_or(row.peer_id),
            node_id: row.node_id,
            source_name,
            peer_url: resolved_peer.map(|p| p.url.clone()),
            available_copies: row.available_copies,
            source_ref_id,
        });
    }

    matches.sort_by(|a, b| a.source_name.cmp(&b.source_name).then(a.isbn.cmp(&b.isbn)));
    Ok(matches)
}

/// The wishlist join: providers for the current borrow-eligible wishlist
/// entries, with the local book attached. `isbn` narrows to a single wish
/// (book details screen); `None` returns every match (wishlist filter).
pub async fn matches_for_wishlist(
    db: &DatabaseConnection,
    isbn: Option<&str>,
) -> Result<Vec<WishlistProviderMatch>, DbErr> {
    let books = wanting_books(db).await?;
    let mut by_isbn: HashMap<String, (String, String)> = HashMap::new();
    for b in books {
        if let Some(i) = b.isbn
            && isbn.is_none_or(|only| only == i)
        {
            by_isbn.insert(i, (b.id, b.title));
        }
    }
    let isbn_set: HashSet<String> = by_isbn.keys().cloned().collect();
    let mut matches = providers_for_isbns(db, &isbn_set).await?;
    for m in &mut matches {
        if let Some((id, title)) = by_isbn.get(&m.isbn) {
            m.book_id = Some(id.clone());
            m.book_title = Some(title.clone());
        }
    }
    Ok(matches)
}

/// Reverse trigger: a wish just entered the library (created as 'wanting'
/// or updated into it). Scans the WHOLE cache for that ISBN and emits one
/// wishlist_match per source.
///
/// Deliberately event-driven, never a periodic sweep: the notification
/// table is pruned by TTL/cap, so a sweep would re-emit forever after each
/// purge (notified_at on peer_books cannot help, it tracks the peer-entry
/// direction, not the wish/provider pair). Deliberately NOT filtered on
/// peer_books.notified_at either: a cache row notified months ago for the
/// "new books" flow is exactly the row this direction must catch.
///
/// The caller guarantees the lane rule (wanting, non-private, has ISBN).
/// Failures are logged, never propagated (same contract as
/// notification_service::emit).
pub async fn notify_providers_for_wish(db: &DatabaseConnection, isbn: &str, title: &str) {
    let mut isbns = HashSet::new();
    isbns.insert(isbn.to_string());
    let matches = match providers_for_isbns(db, &isbns).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("wishlist provider scan failed: {e:?}");
            return;
        }
    };
    for m in matches {
        crate::services::notification_service::emit_unique(
            db,
            CreateNotification {
                event_type: NotificationEventType::WishlistMatch,
                title: title.to_string(),
                body: Some(m.source_name.clone()),
                ref_type: Some("peer".to_string()),
                ref_id: Some(format!("{}:{}", m.source_ref_id, isbn)),
            },
        )
        .await;
    }
}

/// Collapse the per-book wishlist_match notifications emitted while a
/// curated list was importing into ONE aggregated notification for the
/// whole batch. Returns the number of distinct matched ISBNs.
///
/// The aggregated notification reuses the wishlist_match event type
/// (no new Rust variant, no new Dart switch case): ref_type = "import",
/// ref_id = the created collection id, title = the list title, body = the
/// match count. Tapping it falls back to the wishlist filter, where the
/// per-book availability badges take over.
pub async fn aggregate_import_matches(
    db: &DatabaseConnection,
    batch_ref: &str,
    list_title: &str,
    isbns: &[String],
) -> Result<usize, DbErr> {
    // Re-check through the books lane: only ISBNs that landed as actual
    // borrow-eligible wishes (wanting, non-private) count.
    let requested: HashSet<String> = isbns.iter().cloned().collect();
    let wanted: HashSet<String> = wanting_isbn_set(db)
        .await?
        .intersection(&requested)
        .cloned()
        .collect();
    let matches = providers_for_isbns(db, &wanted).await?;
    if matches.is_empty() {
        return Ok(0);
    }

    // Remove the unitary notifications the per-book create trigger emitted
    // for this batch; the aggregate replaces them. The composite
    // "{source}:{isbn}" ref is specific to wishlist matches, so this cannot
    // touch other peer-scoped notifications.
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    for m in &matches {
        let _ = repo
            .dismiss_by_ref("peer", &format!("{}:{}", m.source_ref_id, m.isbn))
            .await;
    }

    let matched_isbns: HashSet<&str> = matches.iter().map(|m| m.isbn.as_str()).collect();
    let count = matched_isbns.len();
    crate::services::notification_service::emit_unique(
        db,
        CreateNotification {
            event_type: NotificationEventType::WishlistMatch,
            title: list_title.to_string(),
            body: Some(count.to_string()),
            ref_type: Some("import".to_string()),
            ref_id: Some(batch_ref.to_string()),
        },
    )
    .await;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Set;

    async fn test_db() -> DatabaseConnection {
        crate::db::init_db("sqlite::memory:").await.unwrap()
    }

    async fn insert_wish(db: &DatabaseConnection, title: &str, isbn: &str, private: bool) {
        let now = chrono::Utc::now().to_rfc3339();
        book::Entity::insert(book::ActiveModel {
            id: Set(uuid::Uuid::new_v4().to_string()),
            title: Set(title.to_owned()),
            isbn: Set(Some(isbn.to_owned())),
            reading_status: Set("wanting".to_owned()),
            owned: Set(false),
            private: Set(private),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
    }

    async fn insert_peer(db: &DatabaseConnection, name: &str, uuid: Option<&str>) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let res = peer::Entity::insert(peer::ActiveModel {
            name: Set(name.to_owned()),
            url: Set(format!("http://{name}.local:8000")),
            library_uuid: Set(uuid.map(str::to_owned)),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        res.last_insert_id
    }

    async fn insert_peer_book(
        db: &DatabaseConnection,
        peer_id: i32,
        node_id: Option<&str>,
        isbn: &str,
        owned: bool,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        // Directory rows use the peer_id = 0 sentinel. Production writes
        // them on a dedicated FK-off connection (api/frb/hub_catalog.rs);
        // the test emulates that by parking a placeholder peers row at id 0
        // (never consulted: the resolution path ignores peers for id 0).
        if peer_id == 0 {
            use sea_orm::sea_query::OnConflict;
            let _ = peer::Entity::insert(peer::ActiveModel {
                id: Set(0),
                name: Set("directory-sentinel".to_owned()),
                url: Set("sentinel://0".to_owned()),
                created_at: Set(now.clone()),
                updated_at: Set(now.clone()),
                ..Default::default()
            })
            .on_conflict(OnConflict::column(peer::Column::Id).do_nothing().to_owned())
            .exec(db)
            .await;
        }
        peer_book::Entity::insert(peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set(uuid::Uuid::new_v4().to_string()),
            title: Set(format!("Peer copy of {isbn}")),
            isbn: Set(Some(isbn.to_owned())),
            synced_at: Set(now),
            node_id: Set(node_id.map(str::to_owned)),
            owned: Set(owned),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
    }

    async fn notification_count(db: &DatabaseConnection, event_type: &str) -> usize {
        use crate::domain::NotificationRepository;
        let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
        repo.list(None, 0, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|n| n.event_type == event_type)
            .count()
    }

    /// A private wanted book must not appear in the shared join: the match
    /// leaks the wish outward (notification names the source, borrow button
    /// sends title + ISBN to the peer).
    #[tokio::test]
    async fn private_wish_is_excluded_from_the_join() {
        let db = test_db().await;
        insert_wish(&db, "Secret wish", "9780000000001", true).await;
        insert_wish(&db, "Public wish", "9780000000002", false).await;
        let peer_id = insert_peer(&db, "Marie", None).await;
        insert_peer_book(&db, peer_id, None, "9780000000001", true).await;
        insert_peer_book(&db, peer_id, None, "9780000000002", true).await;

        let matches = matches_for_wishlist(&db, None).await.unwrap();
        assert_eq!(matches.len(), 1, "only the public wish may match");
        assert_eq!(matches[0].isbn, "9780000000002");

        // Same rule for the forward pass, through wanting_isbn_set.
        let set = wanting_isbn_set(&db).await.unwrap();
        assert!(!set.contains("9780000000001"));
        assert!(set.contains("9780000000002"));
    }

    /// A cache row the peer does not own (they borrowed it themselves) is
    /// not borrowable and must not match.
    #[tokio::test]
    async fn non_owned_cache_rows_never_match() {
        let db = test_db().await;
        insert_wish(&db, "Wish", "9780000000003", false).await;
        let peer_id = insert_peer(&db, "Marie", None).await;
        insert_peer_book(&db, peer_id, None, "9780000000003", false).await;

        let matches = matches_for_wishlist(&db, None).await.unwrap();
        assert!(matches.is_empty(), "owned=false must never match");
    }

    /// A library both paired and followed yields two cache rows for the
    /// same book; the join returns one match, resolved to the paired peer.
    #[tokio::test]
    async fn paired_and_followed_library_dedups_to_one_match() {
        let db = test_db().await;
        insert_wish(&db, "Wish", "9780000000004", false).await;
        let peer_id = insert_peer(&db, "Marie", Some("node-uuid-1")).await;
        insert_peer_book(&db, peer_id, None, "9780000000004", true).await;
        insert_peer_book(&db, 0, Some("node-uuid-1"), "9780000000004", true).await;

        let matches = matches_for_wishlist(&db, None).await.unwrap();
        assert_eq!(matches.len(), 1, "same source must dedup");
        assert_eq!(matches[0].source_name, "Marie");
        assert_eq!(matches[0].source_ref_id, peer_id.to_string());
        assert!(matches[0].peer_url.is_some());
    }

    /// A followed-but-not-paired library resolves to a "dir:{node}" ref
    /// with no peer URL (hub borrow path only).
    #[tokio::test]
    async fn unpaired_directory_entry_uses_dir_ref() {
        let db = test_db().await;
        insert_wish(&db, "Wish", "9780000000005", false).await;
        insert_peer_book(&db, 0, Some("node-uuid-2"), "9780000000005", true).await;

        let matches = matches_for_wishlist(&db, None).await.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source_ref_id, "dir:node-uuid-2");
        assert!(matches[0].peer_url.is_none());
    }

    /// Reverse trigger: the wish entering scans the WHOLE cache, including
    /// rows whose notified_at was set long ago by the forward pass.
    #[tokio::test]
    async fn reverse_trigger_ignores_notified_at() {
        let db = test_db().await;
        let peer_id = insert_peer(&db, "Marie", None).await;
        // Simulate a cache row already notified by the "new books" pass.
        let now = chrono::Utc::now().to_rfc3339();
        peer_book::Entity::insert(peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set("r1".to_owned()),
            title: Set("Old cache row".to_owned()),
            isbn: Set(Some("9780000000006".to_owned())),
            synced_at: Set(now.clone()),
            notified_at: Set(Some(now)),
            owned: Set(true),
            ..Default::default()
        })
        .exec(&db)
        .await
        .unwrap();

        notify_providers_for_wish(&db, "9780000000006", "New wish").await;
        assert_eq!(notification_count(&db, "wishlist_match").await, 1);

        // Re-firing the trigger dedups against the existing notification.
        notify_providers_for_wish(&db, "9780000000006", "New wish").await;
        assert_eq!(notification_count(&db, "wishlist_match").await, 1);
    }

    /// Import aggregation: unitary notifications from the batch collapse
    /// into a single wishlist_match with ref_type = "import".
    #[tokio::test]
    async fn import_aggregation_collapses_unitary_notifications() {
        let db = test_db().await;
        let peer_id = insert_peer(&db, "Marie", None).await;
        let isbns = ["9780000000007", "9780000000008", "9780000000009"];
        for isbn in &isbns {
            insert_wish(&db, isbn, isbn, false).await;
            insert_peer_book(&db, peer_id, None, isbn, true).await;
            // What the per-book create trigger emits during the import loop.
            notify_providers_for_wish(&db, isbn, isbn).await;
        }
        assert_eq!(notification_count(&db, "wishlist_match").await, 3);

        let all: Vec<String> = isbns.iter().map(|s| s.to_string()).collect();
        let count = aggregate_import_matches(&db, "batch-1", "Ma liste", &all)
            .await
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            notification_count(&db, "wishlist_match").await,
            1,
            "the aggregate must replace the unitary notifications"
        );

        use crate::domain::NotificationRepository;
        let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
        let rows = repo.list(None, 0, 10).await.unwrap();
        let agg = rows
            .iter()
            .find(|n| n.event_type == "wishlist_match")
            .unwrap();
        assert_eq!(agg.ref_type.as_deref(), Some("import"));
        assert_eq!(agg.ref_id.as_deref(), Some("batch-1"));
        assert_eq!(agg.title, "Ma liste");
        assert_eq!(agg.body.as_deref(), Some("3"));
    }

    /// An import with zero matches emits nothing at all.
    #[tokio::test]
    async fn import_with_no_match_stays_silent() {
        let db = test_db().await;
        insert_wish(&db, "Wish", "9780000000010", false).await;
        let count =
            aggregate_import_matches(&db, "batch-2", "Liste vide", &["9780000000010".to_string()])
                .await
                .unwrap();
        assert_eq!(count, 0);
        assert_eq!(notification_count(&db, "wishlist_match").await, 0);
    }
}
