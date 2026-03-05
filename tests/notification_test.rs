//! Integration tests for the notification repository.

use rust_lib_app::domain::notification_repository::{
    CreateNotification, MAX_NOTIFICATIONS, NotificationEventType, NotificationRepository,
};
use rust_lib_app::infrastructure::SeaOrmNotificationRepository;
use rust_lib_app::infrastructure::db::init_db;

async fn setup() -> SeaOrmNotificationRepository {
    let db = init_db("sqlite::memory:").await.unwrap();
    SeaOrmNotificationRepository::new(db)
}

fn make_input(evt: NotificationEventType, title: &str) -> CreateNotification {
    CreateNotification {
        event_type: evt,
        title: title.to_string(),
        body: None,
        ref_type: None,
        ref_id: None,
    }
}

// -- Basic CRUD --

#[tokio::test(flavor = "multi_thread")]
async fn test_create_and_list() {
    let repo = setup().await;

    let row = repo
        .create(make_input(
            NotificationEventType::ConnectionRequest,
            "Alice wants to connect",
        ))
        .await
        .unwrap();

    assert_eq!(row.event_type, "connection_request");
    assert_eq!(row.category, "connections");
    assert_eq!(row.title, "Alice wants to connect");
    assert!(row.read_at.is_none());

    let all = repo.list(None, 0, 100).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, row.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_filtered_by_category() {
    let repo = setup().await;

    repo.create(make_input(
        NotificationEventType::ConnectionRequest,
        "Connection",
    ))
    .await
    .unwrap();

    repo.create(make_input(NotificationEventType::BorrowRequest, "Loan"))
        .await
        .unwrap();

    repo.create(make_input(NotificationEventType::WishlistMatch, "Wishlist"))
        .await
        .unwrap();

    let connections = repo.list(Some("connections"), 0, 100).await.unwrap();
    assert_eq!(connections.len(), 1);
    assert_eq!(connections[0].title, "Connection");

    let loans = repo.list(Some("loans"), 0, 100).await.unwrap();
    assert_eq!(loans.len(), 1);
    assert_eq!(loans[0].title, "Loan");

    let discoveries = repo.list(Some("discoveries"), 0, 100).await.unwrap();
    assert_eq!(discoveries.len(), 1);
    assert_eq!(discoveries[0].title, "Wishlist");

    let all = repo.list(None, 0, 100).await.unwrap();
    assert_eq!(all.len(), 3);
}

// -- Unread count --

#[tokio::test(flavor = "multi_thread")]
async fn test_unread_count() {
    let repo = setup().await;

    assert_eq!(repo.unread_count(None).await.unwrap(), 0);

    let r1 = repo
        .create(make_input(NotificationEventType::NewBooks, "New books"))
        .await
        .unwrap();
    repo.create(make_input(
        NotificationEventType::BorrowAccepted,
        "Accepted",
    ))
    .await
    .unwrap();

    assert_eq!(repo.unread_count(None).await.unwrap(), 2);
    assert_eq!(repo.unread_count(Some("discoveries")).await.unwrap(), 1);
    assert_eq!(repo.unread_count(Some("loans")).await.unwrap(), 1);

    // Mark one as read
    repo.mark_read(r1.id).await.unwrap();
    assert_eq!(repo.unread_count(None).await.unwrap(), 1);
}

// -- Mark read --

#[tokio::test(flavor = "multi_thread")]
async fn test_mark_read() {
    let repo = setup().await;

    let row = repo
        .create(make_input(NotificationEventType::NewBooks, "Books"))
        .await
        .unwrap();

    assert!(repo.mark_read(row.id).await.unwrap());
    // Re-list and check read_at is set
    let rows = repo.list(None, 0, 100).await.unwrap();
    assert!(rows[0].read_at.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mark_read_nonexistent_returns_false() {
    let repo = setup().await;
    assert!(!repo.mark_read(9999).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mark_all_read() {
    let repo = setup().await;

    repo.create(make_input(NotificationEventType::NewBooks, "A"))
        .await
        .unwrap();
    repo.create(make_input(NotificationEventType::BorrowRequest, "B"))
        .await
        .unwrap();

    assert_eq!(repo.unread_count(None).await.unwrap(), 2);

    let updated = repo.mark_all_read().await.unwrap();
    assert_eq!(updated, 2);
    assert_eq!(repo.unread_count(None).await.unwrap(), 0);
}

// -- Dismiss --

#[tokio::test(flavor = "multi_thread")]
async fn test_dismiss() {
    let repo = setup().await;

    let row = repo
        .create(make_input(NotificationEventType::NewBooks, "To delete"))
        .await
        .unwrap();

    assert!(repo.dismiss(row.id).await.unwrap());
    assert_eq!(repo.list(None, 0, 100).await.unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_dismiss_nonexistent_returns_false() {
    let repo = setup().await;
    assert!(!repo.dismiss(9999).await.unwrap());
}

// -- Exists (dedup) --

#[tokio::test(flavor = "multi_thread")]
async fn test_exists_dedup() {
    let repo = setup().await;

    repo.create(CreateNotification {
        event_type: NotificationEventType::WishlistMatch,
        title: "Match".to_string(),
        body: None,
        ref_type: Some("peer".to_string()),
        ref_id: Some("peer1:978123".to_string()),
    })
    .await
    .unwrap();

    assert!(
        repo.exists("wishlist_match", "peer", "peer1:978123")
            .await
            .unwrap()
    );
    assert!(
        !repo
            .exists("wishlist_match", "peer", "peer1:978999")
            .await
            .unwrap()
    );
    assert!(
        !repo
            .exists("new_books", "peer", "peer1:978123")
            .await
            .unwrap()
    );
}

// -- Cap pruning --

#[tokio::test(flavor = "multi_thread")]
async fn test_prune_respects_cap() {
    let repo = setup().await;

    // Insert MAX_NOTIFICATIONS + 5 items
    for i in 0..(MAX_NOTIFICATIONS + 5) {
        // Bypass auto-prune by creating directly; the create method prunes,
        // so the cap should be enforced automatically.
        repo.create(make_input(
            NotificationEventType::NewBooks,
            &format!("Book {}", i),
        ))
        .await
        .unwrap();
    }

    let all = repo.list(None, 0, 500).await.unwrap();
    assert!(
        all.len() <= MAX_NOTIFICATIONS as usize,
        "Expected at most {} notifications, got {}",
        MAX_NOTIFICATIONS,
        all.len()
    );
}

// -- Pagination --

#[tokio::test(flavor = "multi_thread")]
async fn test_list_pagination() {
    let repo = setup().await;

    for i in 0..5 {
        repo.create(make_input(
            NotificationEventType::NewBooks,
            &format!("Item {}", i),
        ))
        .await
        .unwrap();
    }

    let page1 = repo.list(None, 0, 2).await.unwrap();
    assert_eq!(page1.len(), 2);

    let page2 = repo.list(None, 2, 2).await.unwrap();
    assert_eq!(page2.len(), 2);

    let page3 = repo.list(None, 4, 2).await.unwrap();
    assert_eq!(page3.len(), 1);

    // No overlap
    let ids1: Vec<i32> = page1.iter().map(|r| r.id).collect();
    let ids2: Vec<i32> = page2.iter().map(|r| r.id).collect();
    assert!(ids1.iter().all(|id| !ids2.contains(id)));
}

// -- Category auto-derivation --

#[tokio::test(flavor = "multi_thread")]
async fn test_category_auto_derived_from_event_type() {
    let repo = setup().await;

    let r = repo
        .create(make_input(
            NotificationEventType::ConnectionAccepted,
            "Accepted",
        ))
        .await
        .unwrap();
    assert_eq!(r.category, "connections");

    let r = repo
        .create(make_input(NotificationEventType::BookReturned, "Returned"))
        .await
        .unwrap();
    assert_eq!(r.category, "loans");

    let r = repo
        .create(make_input(NotificationEventType::WishlistMatch, "Match"))
        .await
        .unwrap();
    assert_eq!(r.category, "discoveries");
}
