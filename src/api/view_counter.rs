// View counter middleware for tracking peer library consultations.
//
// Counts interactive views (book browsing, search) from remote peers.
// Ignores localhost requests (only remote peers count).
// Uses a 15-minute cooldown per source IP to prevent spam.

use axum::{body::Body, extract::ConnectInfo, http::Request, middleware::Next, response::Response};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const COOLDOWN_DURATION: Duration = Duration::from_secs(900); // 15 minutes

/// In-memory cooldown tracker (IP -> last counted timestamp)
#[derive(Clone, Default)]
pub struct ViewCooldownTracker {
    entries: Arc<RwLock<HashMap<IpAddr, Instant>>>,
}

impl ViewCooldownTracker {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if an IP is in cooldown. If not, record it and return true (should count).
    async fn should_count(&self, ip: IpAddr) -> bool {
        let now = Instant::now();

        // Fast path: read-only check
        {
            let map = self.entries.read().await;
            if let Some(&last) = map.get(&ip)
                && now.duration_since(last) < COOLDOWN_DURATION
            {
                return false;
            }
        }

        // Slow path: write lock to update
        let mut map = self.entries.write().await;

        // Double-check after acquiring write lock
        if let Some(&last) = map.get(&ip)
            && now.duration_since(last) < COOLDOWN_DURATION
        {
            return false;
        }

        map.insert(ip, now);

        // Purge expired entries while we hold the lock (bounded cleanup)
        if map.len() > 100 {
            map.retain(|_, last| now.duration_since(*last) < COOLDOWN_DURATION);
        }

        true
    }
}

/// Returns true if the request path is a countable interactive route.
/// Handles both nested paths (after .nest("/api", ...)) and full paths.
fn is_countable_path(path: &str) -> bool {
    // Strip /api prefix if present (middleware may run inside nested router)
    let path = path.strip_prefix("/api").unwrap_or(path);

    if path == "/books" || path == "/books/search" {
        return true;
    }
    // /books/:id - numeric ID after /books/
    if let Some(rest) = path.strip_prefix("/books/") {
        return rest.parse::<i32>().is_ok();
    }
    false
}

/// Returns true if the address is a loopback/localhost address.
fn is_localhost(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Axum middleware that counts interactive library views from remote peers.
/// Reads ViewCooldownTracker and DatabaseConnection from request extensions.
/// ConnectInfo is available when the server uses into_make_service_with_connect_info.
pub async fn view_counter_middleware(request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // Only count GET requests to interactive routes
    if method == axum::http::Method::GET && is_countable_path(&path) {
        let connect_info = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .cloned();

        if let Some(ConnectInfo(addr)) = connect_info {
            let ip = addr.ip();
            if !is_localhost(ip) {
                let tracker = request.extensions().get::<ViewCooldownTracker>().cloned();
                let db = request.extensions().get::<DatabaseConnection>().cloned();

                if let (Some(tracker), Some(db)) = (tracker, db) {
                    // Fire-and-forget: record the view asynchronously
                    tokio::spawn(async move {
                        if tracker.should_count(ip).await {
                            record_peer_view(&db).await;
                        }
                    });
                }
            }
        }
    }

    next.run(request).await
}

/// Upsert a peer view count for today.
pub async fn record_peer_view(db: &DatabaseConnection) {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let sql = r#"INSERT INTO library_view_stats (date, source, count)
                 VALUES (?1, 'peer', 1)
                 ON CONFLICT(date, source)
                 DO UPDATE SET count = count + 1"#;
    let _ = db
        .execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            sql,
            vec![today.into()],
        ))
        .await;
}

/// Query view stats from the database.
/// Returns JSON string with total_peer, total_follower, and daily breakdown.
pub async fn get_view_stats(db: &DatabaseConnection) -> Result<String, String> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT date, source, count FROM library_view_stats ORDER BY date DESC LIMIT 365"
                .to_owned(),
        ))
        .await
        .map_err(|e| format!("Failed to query view stats: {e}"))?;

    let mut total_peer: i64 = 0;
    let mut total_follower: i64 = 0;
    let mut daily: Vec<serde_json::Value> = Vec::new();

    for row in &rows {
        let date: String = row.try_get("", "date").map_err(|e| format!("date: {e}"))?;
        let source: String = row
            .try_get("", "source")
            .map_err(|e| format!("source: {e}"))?;
        let count: i32 = row
            .try_get("", "count")
            .map_err(|e| format!("count: {e}"))?;

        match source.as_str() {
            "peer" => total_peer += count as i64,
            "follower" => total_follower += count as i64,
            _ => {}
        }

        daily.push(serde_json::json!({
            "date": date,
            "source": source,
            "count": count,
        }));
    }

    let result = serde_json::json!({
        "total_peer": total_peer,
        "total_follower": total_follower,
        "total": total_peer + total_follower,
        "daily": daily,
    });

    serde_json::to_string(&result).map_err(|e| format!("JSON serialize error: {e}"))
}

/// Record follower views from hub (called when syncing hub profile data).
pub async fn record_follower_views(db: &DatabaseConnection, count: i64) -> Result<(), String> {
    if count <= 0 {
        return Ok(());
    }
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let sql = r#"INSERT INTO library_view_stats (date, source, count)
                 VALUES (?1, 'follower', ?2)
                 ON CONFLICT(date, source)
                 DO UPDATE SET count = ?2"#;
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        vec![today.into(), count.into()],
    ))
    .await
    .map_err(|e| format!("Failed to record follower views: {e}"))?;
    Ok(())
}

/// HTTP handler for GET /api/stats/views
pub async fn get_view_stats_handler(
    axum::extract::State(state): axum::extract::State<crate::infrastructure::AppState>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, axum::Json<serde_json::Value>)>
{
    let stats_json = get_view_stats(state.db()).await.map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e})),
        )
    })?;

    let value: serde_json::Value = serde_json::from_str(&stats_json).map_err(|e| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("JSON parse error: {e}")})),
        )
    })?;

    Ok(axum::Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_countable_path() {
        // Without /api prefix (nested router)
        assert!(is_countable_path("/books"));
        assert!(is_countable_path("/books/42"));
        assert!(is_countable_path("/books/search"));
        assert!(!is_countable_path("/books/tags"));
        assert!(!is_countable_path("/contacts"));
        assert!(!is_countable_path("/peers"));

        // With /api prefix (full path)
        assert!(is_countable_path("/api/books"));
        assert!(is_countable_path("/api/books/42"));
        assert!(is_countable_path("/api/books/search"));
        assert!(!is_countable_path("/api/books/tags"));
        assert!(!is_countable_path("/api/contacts"));
        assert!(!is_countable_path("/health"));
    }

    #[test]
    fn test_is_localhost() {
        assert!(is_localhost("127.0.0.1".parse().unwrap()));
        assert!(is_localhost("::1".parse().unwrap()));
        assert!(!is_localhost("192.168.1.1".parse().unwrap()));
        assert!(!is_localhost("10.0.0.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_cooldown_tracker() {
        let tracker = ViewCooldownTracker::new();
        let ip: IpAddr = "192.168.1.100".parse().unwrap();

        // First call should count
        assert!(tracker.should_count(ip).await);
        // Second call within cooldown should not count
        assert!(!tracker.should_count(ip).await);

        // Different IP should count
        let ip2: IpAddr = "192.168.1.101".parse().unwrap();
        assert!(tracker.should_count(ip2).await);
    }
}
