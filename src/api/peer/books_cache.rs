//! Cached peer book listings, cache maintenance and the cover proxy.

use super::*;
use crate::models::{peer, peer_gamification_stats};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::Deserialize;
use serde_json::json;

/// Query params for the cover-proxy endpoint.
#[derive(Deserialize)]
pub struct CoverProxyQuery {
    pub peer_url: String,
    pub book_id: i32,
}

/// GET /api/peers/cover-proxy?peer_url={url}&book_id={id}
///
/// Proxies a cover image fetch through the local Rust backend so that
/// Flutter does not make direct HTTP calls to the peer (which fail on
/// iOS/macOS due to firewall, ATS, or NAT issues).
pub async fn cover_proxy(
    State(db): State<DatabaseConnection>,
    axum::extract::Query(params): axum::extract::Query<CoverProxyQuery>,
) -> Result<axum::response::Response, StatusCode> {
    let peer_url = validate_url(&params.peer_url).map_err(|_| StatusCode::BAD_REQUEST)?;
    ensure_registered_peer(&db, &peer_url).await?;
    let peer_url = peer_url.trim_end_matches('/');
    let url = format!("{}/api/books/{}/cover", peer_url, params.book_id);

    let client = get_safe_client();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if !resp.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Cap response size at 10 MB to prevent memory exhaustion from a
    // malicious or misconfigured peer streaming an oversized payload.
    const MAX_COVER_BYTES: usize = 10 * 1024 * 1024;

    if let Some(cl) = resp.content_length()
        && cl as usize > MAX_COVER_BYTES
    {
        return Err(StatusCode::BAD_GATEWAY);
    }

    let bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    if bytes.len() > MAX_COVER_BYTES {
        return Err(StatusCode::BAD_GATEWAY);
    }

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, "public, max-age=3600")
        .body(axum::body::Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn list_peer_books(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Check if peer is approved
    if let Ok(Some(peer)) = peer::Entity::find_by_id(peer_id).one(&db).await
        && !is_peer_approved(&db, &peer).await
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// List peer books by URL (solves ID mismatch)
pub async fn list_peer_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

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

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found with URL: {}", docker_url) })),
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

    // Get books for this peer
    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// Get cached peer books with staleness metadata (no network call to peer)
/// Returns books from local cache along with last_synced timestamp for UI staleness indicator
pub async fn get_cached_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

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

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Peer not found - return empty result with null metadata
            return (
                StatusCode::OK,
                Json(json!({
                    "books": [],
                    "peer_name": null,
                    "peer_id": null,
                    "last_synced": null,
                    "last_seen": null,
                    "cached": true
                })),
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

    // Get cached books for this peer
    let cached = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Get latest synced_at from cached books (all books have same sync time)
    let last_synced = cached.first().map(|b| b.synced_at.clone());

    // Convert peer_book rows to Book DTOs so id == remote_book_id (matches the
    // live P2P shape) and first_seen_at flows through for the "new" badge.
    let books: Vec<crate::models::Book> = cached.into_iter().map(Into::into).collect();

    (
        StatusCode::OK,
        Json(json!({
            "books": books,
            "peer_name": peer.name,
            "peer_id": peer.id,
            "last_synced": last_synced,
            "last_seen": peer.last_seen,
            "cached": true
        })),
    )
        .into_response()
}

/// Cleanup peer_books entries older than 30 days (TTL for privacy)
/// Call this on app startup to auto-purge stale caches
pub async fn cleanup_stale_peer_books(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::peer_book;
    use sea_orm::QueryFilter;

    // Calculate cutoff date (30 days ago)
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let cutoff_str = cutoff.to_rfc3339();

    // Delete stale peer_books entries. Directory catalog rows (peer_id = 0
    // sentinel) are exempt: they mirror a catalog the owner deliberately
    // published to the hub (no privacy concern), they are the only offline
    // fallback once the hub no longer serves that catalog, and they have
    // their own lifecycle (refreshed and pruned by the directory upsert,
    // purged on peer deletion per ADR-024).
    let books_deleted = peer_book::Entity::delete_many()
        .filter(peer_book::Column::SyncedAt.lt(&cutoff_str))
        .filter(peer_book::Column::PeerId.ne(0))
        .exec(&db)
        .await
        .map(|r| r.rows_affected)
        .unwrap_or(0);

    // Also clean up stale peer_gamification_stats
    let stats_deleted = peer_gamification_stats::Entity::delete_many()
        .filter(peer_gamification_stats::Column::SyncedAt.lt(&cutoff_str))
        .exec(&db)
        .await
        .map(|r| r.rows_affected)
        .unwrap_or(0);

    if books_deleted > 0 || stats_deleted > 0 {
        tracing::info!(
            "TTL cleanup: deleted {} stale peer_books + {} stale peer_gamification_stats (older than 30 days)",
            books_deleted,
            stats_deleted
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "deleted": books_deleted,
            "stats_deleted": stats_deleted,
            "cutoff": cutoff_str
        })),
    )
        .into_response()
}

/// Save pre-fetched books to the local peer_books cache.
///
/// Called by Flutter after loading books via relay or live WiFi fetch,
/// so the Rust backend does not need to re-fetch from the remote peer.
/// Input: { "books": [{ "id": 5, "title": "...", ... }, ...] }
pub async fn cache_books_by_id(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // 1. Validate peer exists
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found: {}", peer_id) })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("DB error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Parse books array from payload
    let books: Vec<crate::models::Book> = match payload.get("books") {
        Some(books_val) => serde_json::from_value(books_val.clone()).unwrap_or_default(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'books' field" })),
            )
                .into_response();
        }
    };

    // 3-4. Upsert books cache (preserves first_seen_at).
    // The caller (Flutter) sets `is_full_snapshot` only when it has loaded the
    // peer's entire catalog (all pages / a completed relay page loop). A
    // partial batch (page 0 while the rest streams in, or a truncated relay
    // fetch) defaults to additive so it never drains the cache.
    let is_full_snapshot = payload
        .get("is_full_snapshot")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let count = upsert_peer_books_cache(&db, peer.id, None, books, is_full_snapshot).await;

    (
        StatusCode::OK,
        Json(json!({ "count": count, "peer_id": peer_id })),
    )
        .into_response()
}

#[cfg(test)]
mod cleanup_stale_peer_books_tests {
    //! The 30-day peer_books TTL is a privacy measure for LAN-synced peer
    //! data. Directory catalog rows (peer_id = 0 sentinel) have their own
    //! lifecycle (refreshed and pruned by the directory upsert, purged on
    //! peer deletion per ADR-024) and are the only offline fallback once the
    //! hub no longer serves a catalog, so the TTL must not delete them.
    use super::*;
    use crate::db;
    use crate::models::peer_book;
    use sea_orm::Set;

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer(db: &DatabaseConnection) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let p = peer::ActiveModel {
            name: Set("test-peer".to_string()),
            url: Set("http://test-peer.local:8080".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        peer::Entity::insert(p)
            .exec(db)
            .await
            .expect("insert peer")
            .last_insert_id
    }

    /// Seeds a peer_books row directly. Directory sentinel rows (peer_id = 0)
    /// have no matching peers row, so the insert runs with FK enforcement off
    /// on a dedicated connection, mirroring the production directory insert.
    async fn seed_row(
        db: &DatabaseConnection,
        peer_id: i32,
        node_id: Option<&str>,
        synced_at: &str,
    ) {
        let mut conn = db.get_sqlite_connection_pool().acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO peer_books \
             (peer_id, remote_book_id, title, isbn, author, cover_url, \
              summary, synced_at, node_id, first_seen_at, added_at, notified_at) \
             VALUES (?, '0', 'Title', '9780306406157', NULL, NULL, NULL, ?, ?, ?, NULL, NULL)",
        )
        .bind(peer_id)
        .bind(synced_at)
        .bind(node_id)
        .bind(synced_at)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await
            .unwrap();
    }

    async fn rows_by_peer_id(db: &DatabaseConnection, peer_id: i32) -> usize {
        peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .all(db)
            .await
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn ttl_spares_directory_rows_and_fresh_lan_rows() {
        let db = setup_db().await;
        let peer_id = insert_peer(&db).await;
        let stale = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        let fresh = chrono::Utc::now().to_rfc3339();

        // Stale directory sentinel row: must survive the TTL.
        seed_row(&db, 0, Some("node-under-test"), &stale).await;
        // Stale LAN row: the TTL's actual target.
        seed_row(&db, peer_id, None, &stale).await;
        // Fresh LAN row: inside the TTL window.
        seed_row(&db, peer_id, None, &fresh).await;

        let _ = cleanup_stale_peer_books(State(db.clone())).await;

        assert_eq!(
            rows_by_peer_id(&db, 0).await,
            1,
            "directory catalog cache (peer_id = 0) must be exempt from the TTL"
        );
        assert_eq!(
            rows_by_peer_id(&db, peer_id).await,
            1,
            "stale LAN row must be deleted, fresh LAN row kept"
        );
    }
}
