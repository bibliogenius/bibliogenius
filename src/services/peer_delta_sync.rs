//! Peer catalog delta sync over E2EE (direct LAN or relay).
//!
//! ADR-029 requester-side orchestrator. Reuses `try_send_e2ee` for transport
//! symmetry (LAN-first, relay fallback with correlation) and
//! `build_book_delta_response` semantics on the responder side.
//!
//! The orchestrator is the single entry point for catalog pulls that want
//! incremental bandwidth. It owns:
//!
//! - Reading the persistent `peers.last_delta_cursor`.
//! - Shaping the `catalog_delta_request` payload.
//! - Blocking on the response (LAN direct or relay correlation, handled by
//!   `try_send_e2ee`).
//! - Applying the returned operations to `peer_books` while preserving the
//!   "new book" badge semantics (`first_seen_at`).
//! - Persisting the fresh cursor only after the apply succeeds, so a failure
//!   mid-batch never advances past unapplied rows.
//!
//! Fallback to the legacy full-catalog flow (`library_manifest_request` +
//! `library_page_request` loop, ADR-012) is reported as a distinct outcome
//! rather than handled here: the caller decides, because the legacy path has
//! UI implications (progress bar restart) that cannot be hidden in a helper.

use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::infrastructure::AppState;
use crate::models::{peer, peer_book};

/// Outcome of one call to `fetch_and_apply_peer_delta`.
///
/// Callers branch on this rather than re-decoding the relay payload. Every
/// variant except `Applied` signals "this pass did not update the cache" and
/// the caller decides whether to fall back, retry, or give up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaSyncOutcome {
    /// Delta batch applied. The cursor has been persisted. If `has_more` is
    /// true the caller MUST re-enter the orchestrator immediately — the
    /// response was capped by `limit`.
    Applied {
        operations_applied: usize,
        latest_cursor: i64,
        has_more: bool,
    },
    /// Responder reported that the cursor predates the oldest retained log
    /// row. The caller must fall back to the legacy full-catalog flow.
    ResetRequired,
    /// Responder did not reply within the transport timeout or sent a
    /// response the orchestrator could not parse. Treat as a fallback
    /// trigger: the peer is likely on an older codebase that does not know
    /// `catalog_delta_request`.
    FallbackRequired,
    /// E2EE is not available for this peer (no keys or crypto not
    /// initialised). Caller should stay on the legacy plaintext path.
    E2eeUnavailable,
}

/// Default scan budget when shaping a `catalog_delta_request`. Bounded by
/// `build_book_delta_response` which re-clamps to `DELTA_MAX_LIMIT`.
const DELTA_REQUEST_LIMIT: i64 = 500;

/// Fetch a delta window from a peer and apply it to the local `peer_books`
/// cache.
///
/// On success, advances `peers.last_delta_cursor` to the returned
/// `latest_cursor`. On any non-success outcome the cursor is left untouched
/// so that a later retry resumes from the last acknowledged position.
pub async fn fetch_and_apply_peer_delta(
    state: &AppState,
    peer_id: i32,
) -> Result<DeltaSyncOutcome, String> {
    let db = state.db();

    let peer_model = peer::Entity::find_by_id(peer_id)
        .one(db)
        .await
        .map_err(|e| format!("load peer {peer_id}: {e}"))?
        .ok_or_else(|| format!("peer {peer_id} not found"))?;

    let since: Option<i64> = peer_model.last_delta_cursor.map(|c| c as i64);

    let payload = serde_json::json!({
        "since": since,
        "limit": DELTA_REQUEST_LIMIT,
    });

    let send_result =
        crate::api::peer::try_send_e2ee(state, &peer_model, "catalog_delta_request", payload).await;

    let response = match send_result {
        Ok(Some(Some(response))) => response,
        Ok(Some(None)) => {
            // Fire-and-forget relay return OR timeout awaiting relay response.
            // For a request-response type (ADR-012 RELAY_AWAIT_RESPONSE)
            // this means no response came back — likely an older peer that
            // does not understand `catalog_delta_request`. Caller falls back
            // to the legacy full path.
            tracing::info!(
                "peer_delta_sync: peer {} did not respond to catalog_delta_request, fallback required",
                peer_model.name
            );
            return Ok(DeltaSyncOutcome::FallbackRequired);
        }
        Ok(None) => {
            tracing::debug!(
                "peer_delta_sync: peer {} has no E2EE capability",
                peer_model.name
            );
            return Ok(DeltaSyncOutcome::E2eeUnavailable);
        }
        Err(e) => return Err(format!("try_send_e2ee: {e}")),
    };

    let reset_required = response
        .payload
        .get("reset_required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if reset_required {
        tracing::info!(
            "peer_delta_sync: peer {} reported reset_required (cursor pruned)",
            peer_model.name
        );
        return Ok(DeltaSyncOutcome::ResetRequired);
    }

    let operations = response
        .payload
        .get("operations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let latest_cursor = response
        .payload
        .get("latest_cursor")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| since.unwrap_or(0));

    let has_more = response
        .payload
        .get("has_more")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // On the very first sync, first_seen_at stays NULL to suppress the "new"
    // badge for the bulk-arriving initial catalog. Subsequent deltas mark
    // each incoming book with "now" so the UI can highlight recent additions.
    let is_initial_sync = since.is_none();

    let applied = apply_peer_delta_operations(db, peer_id, &operations, is_initial_sync)
        .await
        .map_err(|e| format!("apply_peer_delta_operations: {e}"))?;

    persist_peer_cursor(db, peer_id, latest_cursor)
        .await
        .map_err(|e| format!("persist_peer_cursor: {e}"))?;

    Ok(DeltaSyncOutcome::Applied {
        operations_applied: applied,
        latest_cursor,
        has_more,
    })
}

/// Apply a list of delta operations to the `peer_books` cache.
///
/// - `{ "op": "upsert", "book": { ... } }` upserts the row for
///   `(peer_id, remote_book_id)`, preserving `first_seen_at` and
///   `notified_at` on existing rows.
/// - `{ "op": "delete", "book_id": N }` removes the row for
///   `(peer_id, N)`; absent rows are silent no-ops (idempotent).
///
/// Returns the number of operations that made it past basic shape
/// validation. Malformed ops are logged and skipped without aborting.
pub async fn apply_peer_delta_operations(
    db: &DatabaseConnection,
    peer_id: i32,
    operations: &[serde_json::Value],
    is_initial_sync: bool,
) -> Result<usize, sea_orm::DbErr> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut applied = 0usize;

    for op in operations {
        let op_type = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op_type {
            "upsert" => {
                let Some(book_value) = op.get("book") else {
                    tracing::warn!(
                        "peer_delta_sync: skipping upsert without 'book' field for peer {peer_id}"
                    );
                    continue;
                };
                let book: crate::models::Book = match serde_json::from_value(book_value.clone()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            "peer_delta_sync: failed to decode book payload for peer {peer_id}: {e}"
                        );
                        continue;
                    }
                };
                let Some(remote_id) = book.id else {
                    tracing::warn!(
                        "peer_delta_sync: upsert without book.id for peer {peer_id}, skipping"
                    );
                    continue;
                };

                upsert_peer_book_row(db, peer_id, remote_id, &book, &now, is_initial_sync).await?;
                applied += 1;
            }
            "delete" => {
                let Some(book_id) = op.get("book_id").and_then(|v| v.as_i64()) else {
                    tracing::warn!(
                        "peer_delta_sync: delete op without 'book_id' for peer {peer_id}"
                    );
                    continue;
                };
                peer_book::Entity::delete_many()
                    .filter(peer_book::Column::PeerId.eq(peer_id))
                    .filter(peer_book::Column::RemoteBookId.eq(book_id as i32))
                    .exec(db)
                    .await?;
                applied += 1;
            }
            other => {
                tracing::warn!(
                    "peer_delta_sync: unknown op type '{other}' for peer {peer_id}, skipping"
                );
            }
        }
    }

    Ok(applied)
}

async fn upsert_peer_book_row(
    db: &DatabaseConnection,
    peer_id: i32,
    remote_id: i32,
    book: &crate::models::Book,
    now: &str,
    is_initial_sync: bool,
) -> Result<(), sea_orm::DbErr> {
    let existing = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .filter(peer_book::Column::RemoteBookId.eq(remote_id))
        .one(db)
        .await?;

    if let Some(row) = existing {
        let mut active: peer_book::ActiveModel = row.into();
        active.title = Set(book.title.clone());
        active.isbn = Set(book.isbn.clone());
        active.author = Set(book.author.clone());
        active.cover_url = Set(book.cover_url.clone());
        active.summary = Set(book.summary.clone());
        active.synced_at = Set(now.to_string());
        // first_seen_at and notified_at intentionally preserved (ADR-029 C2).
        active.update(db).await?;
    } else {
        let new_row = peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set(remote_id),
            title: Set(book.title.clone()),
            isbn: Set(book.isbn.clone()),
            author: Set(book.author.clone()),
            cover_url: Set(book.cover_url.clone()),
            summary: Set(book.summary.clone()),
            synced_at: Set(now.to_string()),
            node_id: Set(None),
            first_seen_at: Set(if is_initial_sync {
                None
            } else {
                Some(now.to_string())
            }),
            notified_at: Set(None),
            ..Default::default()
        };
        peer_book::Entity::insert(new_row).exec(db).await?;
    }

    Ok(())
}

async fn persist_peer_cursor(
    db: &DatabaseConnection,
    peer_id: i32,
    cursor: i64,
) -> Result<(), sea_orm::DbErr> {
    peer::Entity::update_many()
        .filter(peer::Column::Id.eq(peer_id))
        .col_expr(
            peer::Column::LastDeltaCursor,
            sea_orm::sea_query::Expr::value(cursor.clamp(0, i32::MAX as i64) as i32),
        )
        .col_expr(
            peer::Column::UpdatedAt,
            sea_orm::sea_query::Expr::value(chrono::Utc::now().to_rfc3339()),
        )
        .exec(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use serde_json::json;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:")
            .await
            .expect("init_db in memory")
    }

    async fn create_peer(db: &DatabaseConnection) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let p = peer::ActiveModel {
            name: Set("Test peer".to_owned()),
            url: Set(format!("http://peer-{}.local", uuid::Uuid::new_v4())),
            key_exchange_done: Set(false),
            connection_status: Set("accepted".to_owned()),
            auto_approve: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer");
        p.id
    }

    fn upsert_op(remote_book_id: i32, title: &str) -> serde_json::Value {
        json!({
            "op": "upsert",
            "book": {
                "id": remote_book_id,
                "title": title,
                "isbn": null,
                "author": "Author",
                "cover_url": null,
                "summary": null,
                "owned": true,
                "private": false,
            }
        })
    }

    #[tokio::test]
    async fn apply_upsert_inserts_new_row() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        let ops = vec![upsert_op(42, "Hello World")];
        let applied = apply_peer_delta_operations(&db, peer_id, &ops, false)
            .await
            .unwrap();
        assert_eq!(applied, 1);

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(42))
            .one(&db)
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(row.title, "Hello World");
        assert!(
            row.first_seen_at.is_some(),
            "non-initial sync must stamp first_seen_at",
        );
    }

    #[tokio::test]
    async fn initial_sync_leaves_first_seen_at_null() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        let ops = vec![upsert_op(1, "A"), upsert_op(2, "B")];
        apply_peer_delta_operations(&db, peer_id, &ops, true)
            .await
            .unwrap();

        let rows = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for r in rows {
            assert!(
                r.first_seen_at.is_none(),
                "initial sync must suppress 'new' badge via NULL first_seen_at",
            );
        }
    }

    #[tokio::test]
    async fn apply_upsert_preserves_first_seen_at_on_update() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        let ops = vec![upsert_op(10, "Original")];
        apply_peer_delta_operations(&db, peer_id, &ops, false)
            .await
            .unwrap();
        let first_pass = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(10))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let original_first_seen = first_pass.first_seen_at.clone();
        assert!(original_first_seen.is_some());

        // Second pass with same id but updated title: first_seen_at must be
        // preserved so the "new" badge does not re-fire.
        let ops2 = vec![upsert_op(10, "Renamed")];
        apply_peer_delta_operations(&db, peer_id, &ops2, false)
            .await
            .unwrap();

        let second_pass = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(10))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_pass.title, "Renamed");
        assert_eq!(
            second_pass.first_seen_at, original_first_seen,
            "first_seen_at MUST survive across delta upserts (ADR-029 C2)",
        );
    }

    #[tokio::test]
    async fn apply_delete_removes_row() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        apply_peer_delta_operations(&db, peer_id, &[upsert_op(7, "Doomed")], false)
            .await
            .unwrap();

        let ops = vec![json!({ "op": "delete", "book_id": 7 })];
        let applied = apply_peer_delta_operations(&db, peer_id, &ops, false)
            .await
            .unwrap();
        assert_eq!(applied, 1);

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(7))
            .one(&db)
            .await
            .unwrap();
        assert!(row.is_none(), "delete must remove the peer_books row");
    }

    #[tokio::test]
    async fn apply_delete_on_absent_row_is_noop() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        let ops = vec![json!({ "op": "delete", "book_id": 404 })];
        // Must not error even if the row never existed (idempotent replay).
        let applied = apply_peer_delta_operations(&db, peer_id, &ops, false)
            .await
            .unwrap();
        assert_eq!(applied, 1, "idempotent delete still counts as applied");
    }

    #[tokio::test]
    async fn malformed_op_is_skipped_not_fatal() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        let ops = vec![
            json!({ "op": "upsert" }),              // missing book
            json!({ "op": "delete" }),              // missing book_id
            json!({ "op": "patch", "book_id": 1 }), // unknown op
            upsert_op(99, "Survivor"),
        ];
        let applied = apply_peer_delta_operations(&db, peer_id, &ops, false)
            .await
            .unwrap();
        assert_eq!(applied, 1, "only the well-formed upsert should count");

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(99))
            .one(&db)
            .await
            .unwrap();
        assert!(row.is_some());
    }

    #[tokio::test]
    async fn persist_peer_cursor_updates_row() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        persist_peer_cursor(&db, peer_id, 12345).await.unwrap();

        let reloaded = peer::Entity::find_by_id(peer_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.last_delta_cursor, Some(12345));
    }
}
