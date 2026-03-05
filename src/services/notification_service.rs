//! Notification emission helpers.
//!
//! Provides fire-and-forget functions to create notifications from various
//! event points in the codebase. Failures are logged but never propagated
//! to the caller (notifications must not break the main flow).

use sea_orm::DatabaseConnection;

use crate::domain::notification_repository::{
    CreateNotification, NotificationEventType, NotificationRepository,
};
use crate::infrastructure::SeaOrmNotificationRepository;

/// Emit a notification. Failures are logged, never propagated.
pub async fn emit(db: &DatabaseConnection, input: CreateNotification) {
    let repo = SeaOrmNotificationRepository::new(db.clone());
    if let Err(e) = repo.create(input).await {
        tracing::warn!("notification emit failed: {e:?}");
    }
}

/// Emit a notification only if no duplicate exists (same event_type + ref_type + ref_id).
pub async fn emit_unique(db: &DatabaseConnection, input: CreateNotification) {
    let repo = SeaOrmNotificationRepository::new(db.clone());
    let ref_type = input.ref_type.as_deref().unwrap_or("");
    let ref_id = input.ref_id.as_deref().unwrap_or("");
    if !ref_type.is_empty() && !ref_id.is_empty() {
        match repo
            .exists(input.event_type.as_str(), ref_type, ref_id)
            .await
        {
            Ok(true) => return, // Already exists, skip
            Err(e) => {
                tracing::warn!("notification dedup check failed: {e:?}");
                return;
            }
            Ok(false) => {} // Proceed
        }
    }
    if let Err(e) = repo.create(input).await {
        tracing::warn!("notification emit failed: {e:?}");
    }
}

/// Check newly inserted peer/directory books against the user's wishlist
/// and emit a wishlist_match notification for each match.
///
/// `new_isbns` should contain only ISBNs of books that were just INSERTed
/// (not updated). `source_name` is the peer or library display name.
/// `source_ref_type` is "peer" or "directory".
/// `source_ref_id` is the peer_id or node_id.
pub async fn check_wishlist_matches(
    db: &DatabaseConnection,
    new_isbns: &[(String, String)], // (isbn, title)
    source_name: &str,
    source_ref_type: &str,
    source_ref_id: &str,
) {
    if new_isbns.is_empty() {
        return;
    }

    // Load wishlist ISBNs (books not owned, with ISBN)
    use crate::models::book;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let wishlist: Vec<String> = match book::Entity::find()
        .filter(book::Column::Owned.eq(false))
        .filter(book::Column::Isbn.is_not_null())
        .all(db)
        .await
    {
        Ok(books) => books.into_iter().filter_map(|b| b.isbn).collect(),
        Err(e) => {
            tracing::warn!("wishlist query failed: {e:?}");
            return;
        }
    };

    if wishlist.is_empty() {
        return;
    }

    let wishlist_set: std::collections::HashSet<&str> =
        wishlist.iter().map(|s| s.as_str()).collect();

    for (isbn, title) in new_isbns {
        if wishlist_set.contains(isbn.as_str()) {
            // Deduplicated ref_id: combine source + isbn to avoid duplicate notifications
            let ref_id = format!("{}:{}", source_ref_id, isbn);
            emit_unique(
                db,
                CreateNotification {
                    event_type: NotificationEventType::WishlistMatch,
                    title: title.clone(),
                    body: Some(source_name.to_string()),
                    ref_type: Some(source_ref_type.to_string()),
                    ref_id: Some(ref_id),
                },
            )
            .await;
        }
    }
}
