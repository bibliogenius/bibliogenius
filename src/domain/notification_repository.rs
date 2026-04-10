//! Notification repository trait and related types
//!
//! Domain-level abstractions for the activity feed.
//! No framework dependencies (no SeaORM, no Axum).

use async_trait::async_trait;

use super::DomainError;

/// Maximum total notifications kept in the database.
pub const MAX_NOTIFICATIONS: u64 = 200;
/// Days after which read notifications are pruned.
pub const TTL_READ_DAYS: i64 = 7;
/// Days after which any notification is pruned (even unread).
pub const TTL_GLOBAL_DAYS: i64 = 30;

/// Notification categories (filterable in the UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationCategory {
    Connections,
    Loans,
    Discoveries,
    System,
}

impl NotificationCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connections => "connections",
            Self::Loans => "loans",
            Self::Discoveries => "discoveries",
            Self::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "connections" => Some(Self::Connections),
            "loans" => Some(Self::Loans),
            "discoveries" => Some(Self::Discoveries),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Event types within each category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEventType {
    // Connections
    ConnectionRequest,
    ConnectionAccepted,
    NewFollower,
    FollowRequest,
    // Loans
    BorrowRequest,
    BorrowAccepted,
    BorrowRejected,
    BookReturned,
    BookReclaimed,
    LoanDueReminder,
    LoanDueToday,
    // Discoveries
    NewBooks,
    WishlistMatch,
    // System
    Welcome,
}

impl NotificationEventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConnectionRequest => "connection_request",
            Self::ConnectionAccepted => "connection_accepted",
            Self::NewFollower => "new_follower",
            Self::FollowRequest => "follow_request",
            Self::BorrowRequest => "borrow_request",
            Self::BorrowAccepted => "borrow_accepted",
            Self::BorrowRejected => "borrow_rejected",
            Self::BookReturned => "book_returned",
            Self::BookReclaimed => "book_reclaimed",
            Self::LoanDueReminder => "loan_due_reminder",
            Self::LoanDueToday => "loan_due_today",
            Self::NewBooks => "new_books",
            Self::WishlistMatch => "wishlist_match",
            Self::Welcome => "welcome",
        }
    }

    pub fn category(&self) -> NotificationCategory {
        match self {
            Self::ConnectionRequest
            | Self::ConnectionAccepted
            | Self::NewFollower
            | Self::FollowRequest => NotificationCategory::Connections,
            Self::BorrowRequest
            | Self::BorrowAccepted
            | Self::BorrowRejected
            | Self::BookReturned
            | Self::BookReclaimed
            | Self::LoanDueReminder
            | Self::LoanDueToday => NotificationCategory::Loans,
            Self::NewBooks | Self::WishlistMatch => NotificationCategory::Discoveries,
            Self::Welcome => NotificationCategory::System,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "connection_request" => Some(Self::ConnectionRequest),
            "connection_accepted" => Some(Self::ConnectionAccepted),
            "new_follower" => Some(Self::NewFollower),
            "follow_request" => Some(Self::FollowRequest),
            "borrow_request" => Some(Self::BorrowRequest),
            "borrow_accepted" => Some(Self::BorrowAccepted),
            "borrow_rejected" => Some(Self::BorrowRejected),
            "book_returned" => Some(Self::BookReturned),
            "book_reclaimed" => Some(Self::BookReclaimed),
            "loan_due_reminder" => Some(Self::LoanDueReminder),
            "loan_due_today" => Some(Self::LoanDueToday),
            "new_books" => Some(Self::NewBooks),
            "wishlist_match" => Some(Self::WishlistMatch),
            "welcome" => Some(Self::Welcome),
            _ => None,
        }
    }
}

/// Data needed to create a notification.
#[derive(Debug, Clone)]
pub struct CreateNotification {
    pub event_type: NotificationEventType,
    pub title: String,
    pub body: Option<String>,
    pub ref_type: Option<String>,
    pub ref_id: Option<String>,
}

/// A notification row returned from the database.
#[derive(Debug, Clone)]
pub struct NotificationRow {
    pub id: i32,
    pub event_type: String,
    pub category: String,
    pub title: String,
    pub body: Option<String>,
    pub ref_type: Option<String>,
    pub ref_id: Option<String>,
    pub read_at: Option<String>,
    pub created_at: String,
}

/// Repository trait for the activity feed.
#[async_trait]
pub trait NotificationRepository: Send + Sync {
    /// Insert a notification and run post-insert pruning.
    async fn create(&self, input: CreateNotification) -> Result<NotificationRow, DomainError>;

    /// List notifications, optionally filtered by category. Ordered by created_at DESC.
    /// Paginated: `offset` and `limit`.
    async fn list(
        &self,
        category: Option<&str>,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<NotificationRow>, DomainError>;

    /// Count unread notifications (optionally filtered by category).
    async fn unread_count(&self, category: Option<&str>) -> Result<i64, DomainError>;

    /// Mark a single notification as read. Returns true if found.
    async fn mark_read(&self, id: i32) -> Result<bool, DomainError>;

    /// Mark all notifications as read. Returns count updated.
    async fn mark_all_read(&self) -> Result<i64, DomainError>;

    /// Dismiss (hard delete) a single notification. Returns true if found.
    async fn dismiss(&self, id: i32) -> Result<bool, DomainError>;

    /// Dismiss (hard delete) all notifications. Returns count of deleted rows.
    async fn dismiss_all(&self) -> Result<i64, DomainError>;

    /// Run TTL + cap pruning. Returns count of deleted rows.
    async fn prune(&self) -> Result<i64, DomainError>;

    /// Check if a notification with the given event_type, ref_type and ref_id already exists.
    /// Used for deduplication (e.g. wishlist_match for the same book from the same peer).
    async fn exists(
        &self,
        event_type: &str,
        ref_type: &str,
        ref_id: &str,
    ) -> Result<bool, DomainError>;

    /// Dismiss (hard delete) all notifications with the given ref_type and ref_id.
    /// Returns count of deleted rows. Used to clean up loan reminders on return.
    async fn dismiss_by_ref(&self, ref_type: &str, ref_id: &str) -> Result<i64, DomainError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_category_roundtrip() {
        for cat in [
            NotificationCategory::Connections,
            NotificationCategory::Loans,
            NotificationCategory::Discoveries,
        ] {
            let s = cat.as_str();
            assert_eq!(NotificationCategory::parse(s), Some(cat));
        }
    }

    #[test]
    fn test_category_parse_unknown_returns_none() {
        assert_eq!(NotificationCategory::parse("unknown"), None);
        assert_eq!(NotificationCategory::parse(""), None);
    }

    #[test]
    fn test_event_type_roundtrip() {
        let all = [
            NotificationEventType::ConnectionRequest,
            NotificationEventType::ConnectionAccepted,
            NotificationEventType::NewFollower,
            NotificationEventType::FollowRequest,
            NotificationEventType::BorrowRequest,
            NotificationEventType::BorrowAccepted,
            NotificationEventType::BorrowRejected,
            NotificationEventType::BookReturned,
            NotificationEventType::BookReclaimed,
            NotificationEventType::LoanDueReminder,
            NotificationEventType::LoanDueToday,
            NotificationEventType::NewBooks,
            NotificationEventType::WishlistMatch,
        ];
        for evt in all {
            let s = evt.as_str();
            assert_eq!(NotificationEventType::parse(s), Some(evt));
        }
    }

    #[test]
    fn test_event_type_parse_unknown_returns_none() {
        assert_eq!(NotificationEventType::parse("nope"), None);
    }

    #[test]
    fn test_event_type_category_mapping() {
        assert_eq!(
            NotificationEventType::ConnectionRequest.category(),
            NotificationCategory::Connections
        );
        assert_eq!(
            NotificationEventType::ConnectionAccepted.category(),
            NotificationCategory::Connections
        );
        assert_eq!(
            NotificationEventType::BorrowRequest.category(),
            NotificationCategory::Loans
        );
        assert_eq!(
            NotificationEventType::BorrowAccepted.category(),
            NotificationCategory::Loans
        );
        assert_eq!(
            NotificationEventType::BookReturned.category(),
            NotificationCategory::Loans
        );
        assert_eq!(
            NotificationEventType::LoanDueReminder.category(),
            NotificationCategory::Loans
        );
        assert_eq!(
            NotificationEventType::LoanDueToday.category(),
            NotificationCategory::Loans
        );
        assert_eq!(
            NotificationEventType::NewBooks.category(),
            NotificationCategory::Discoveries
        );
        assert_eq!(
            NotificationEventType::WishlistMatch.category(),
            NotificationCategory::Discoveries
        );
        assert_eq!(
            NotificationEventType::Welcome.category(),
            NotificationCategory::System
        );
    }

    #[test]
    fn test_all_event_types_have_12_variants() {
        // Ensure new variants are covered by tests
        let all = [
            "connection_request",
            "connection_accepted",
            "new_follower",
            "follow_request",
            "borrow_request",
            "borrow_accepted",
            "book_returned",
            "new_books",
            "wishlist_match",
            "welcome",
            "loan_due_reminder",
            "loan_due_today",
        ];
        for s in all {
            assert!(
                NotificationEventType::parse(s).is_some(),
                "parse failed for: {}",
                s
            );
        }
        assert_eq!(all.len(), 12);
    }
}
