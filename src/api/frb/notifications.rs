// Local notification center handlers.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ── Activity Feed (Notifications) ─────────────────────────────────────

#[flutter_rust_bridge::frb]
pub struct FrbNotification {
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

impl From<crate::domain::NotificationRow> for FrbNotification {
    fn from(n: crate::domain::NotificationRow) -> Self {
        Self {
            id: n.id,
            event_type: n.event_type,
            category: n.category,
            title: n.title,
            body: n.body,
            ref_type: n.ref_type,
            ref_id: n.ref_id,
            read_at: n.read_at,
            created_at: n.created_at,
        }
    }
}

/// List notifications, optionally filtered by category.
#[flutter_rust_bridge::frb]
pub async fn notifications_list(
    category: Option<String>,
    offset: u64,
    limit: u64,
) -> Result<Vec<FrbNotification>, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    let rows = repo
        .list(category.as_deref(), offset, limit)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(rows.into_iter().map(FrbNotification::from).collect())
}

/// Get unread notification count (optionally by category).
#[flutter_rust_bridge::frb]
pub async fn notifications_unread_count(category: Option<String>) -> Result<i32, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    repo.unread_count(category.as_deref())
        .await
        .map(|c| c as i32)
        .map_err(|e| format!("{e:?}"))
}

/// Mark a single notification as read.
#[flutter_rust_bridge::frb]
pub async fn notifications_mark_read(id: i32) -> Result<bool, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    repo.mark_read(id).await.map_err(|e| format!("{e:?}"))
}

/// Mark all notifications as read.
#[flutter_rust_bridge::frb]
pub async fn notifications_mark_all_read() -> Result<i32, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    repo.mark_all_read()
        .await
        .map(|c| c as i32)
        .map_err(|e| format!("{e:?}"))
}

/// Dismiss (hard delete) a single notification.
#[flutter_rust_bridge::frb]
pub async fn notifications_dismiss(id: i32) -> Result<bool, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    repo.dismiss(id).await.map_err(|e| format!("{e:?}"))
}

/// Dismiss (hard delete) all notifications. Returns count of deleted rows.
#[flutter_rust_bridge::frb]
pub async fn notifications_dismiss_all() -> Result<i32, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    repo.dismiss_all()
        .await
        .map(|c| c as i32)
        .map_err(|e| format!("{e:?}"))
}

/// Run pruning (TTL + cap). Call on app startup.
#[flutter_rust_bridge::frb]
pub async fn notifications_prune() -> Result<i32, String> {
    use crate::domain::NotificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::SeaOrmNotificationRepository::new(db.clone());
    repo.prune()
        .await
        .map(|c| c as i32)
        .map_err(|e| format!("{e:?}"))
}

/// Emit a one-time welcome notification after setup. Uses emit_unique
/// so it fires at most once per install (idempotent on re-call).
pub async fn emit_welcome_notification() -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::notification_service::emit_unique(
        db,
        crate::domain::CreateNotification {
            event_type: crate::domain::notification_repository::NotificationEventType::Welcome,
            title: "BiblioGenius".to_string(),
            body: None,
            ref_type: Some("system".to_string()),
            ref_id: Some("welcome".to_string()),
        },
    )
    .await;
    Ok(())
}
