// Wishlist provider availability (who has the books I want).
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Wishlist providers ────────────────────────────────────────────────

/// A cached peer/directory book matching a wishlist entry.
#[flutter_rust_bridge::frb]
pub struct FrbWishlistProvider {
    pub isbn: String,
    /// Local wanting book id (None when matching arbitrary ISBNs).
    pub book_id: Option<String>,
    /// Peer id; 0 = directory-only (followed library, not paired).
    pub peer_id: i32,
    pub node_id: Option<String>,
    pub source_name: String,
    /// Paired peer URL for the P2P borrow path; None = hub borrow path.
    pub peer_url: Option<String>,
    pub available_copies: Option<i32>,
}

impl From<crate::services::wishlist_service::WishlistProviderMatch> for FrbWishlistProvider {
    fn from(m: crate::services::wishlist_service::WishlistProviderMatch) -> Self {
        Self {
            isbn: m.isbn,
            book_id: m.book_id,
            peer_id: m.peer_id,
            node_id: m.node_id,
            source_name: m.source_name,
            peer_url: m.peer_url,
            available_copies: m.available_copies,
        }
    }
}

/// Providers for the current wishlist (wanting, non-private, with an ISBN).
/// `isbn` narrows to a single wish (book details); None returns every match
/// (wishlist filter badges).
#[flutter_rust_bridge::frb]
pub async fn get_wishlist_providers(
    isbn: Option<String>,
) -> Result<Vec<FrbWishlistProvider>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::wishlist_service::matches_for_wishlist(db, isbn.as_deref())
        .await
        .map(|v| v.into_iter().map(FrbWishlistProvider::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Providers for arbitrary ISBNs, without going through the local books
/// lane. Used by the curated list import screen to show availability
/// BEFORE the books exist locally.
#[flutter_rust_bridge::frb]
pub async fn get_isbn_providers(isbns: Vec<String>) -> Result<Vec<FrbWishlistProvider>, String> {
    let db = db().ok_or("Database not initialized")?;
    let set: std::collections::HashSet<String> = isbns.into_iter().collect();
    crate::services::wishlist_service::providers_for_isbns(db, &set)
        .await
        .map(|v| v.into_iter().map(FrbWishlistProvider::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Collapse the per-book wishlist_match notifications emitted during a
/// curated list import into one aggregated notification (ref_type =
/// "import", ref_id = batch_ref). Returns the number of matched ISBNs.
#[flutter_rust_bridge::frb]
pub async fn aggregate_wishlist_import_notification(
    batch_ref: String,
    list_title: String,
    isbns: Vec<String>,
) -> Result<i32, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::wishlist_service::aggregate_import_matches(db, &batch_ref, &list_title, &isbns)
        .await
        .map(|c| c as i32)
        .map_err(|e| format!("{e:?}"))
}
