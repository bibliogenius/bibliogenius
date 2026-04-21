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
//! - Applying the returned operations to `peer_books`, storing each book's
//!   owner-supplied `added_at` so the "new book" badge is consistent across
//!   every viewer of the same library.
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
    ///
    /// `current_cursor` is the responder's `operation_log` max id at the
    /// time of the response, when it populated the protocol field. Callers
    /// MUST NOT persist this as their cursor until the full-catalog flow
    /// has rebuilt state — persisting beforehand would skip over history
    /// the client hasn't materialised yet. Once the full flow succeeds, the
    /// caller should call [`set_peer_last_delta_cursor`] with this value so
    /// the next sync can resume as a delta rather than trigger the same
    /// reset loop. `None` when talking to an older responder that did not
    /// populate the field.
    ResetRequired { current_cursor: Option<i64> },
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
        let current_cursor = response
            .payload
            .get("current_cursor")
            .and_then(|v| v.as_i64());
        tracing::info!(
            "peer_delta_sync: peer {} reported reset_required (cursor pruned, responder current_cursor={:?})",
            peer_model.name,
            current_cursor
        );
        return Ok(DeltaSyncOutcome::ResetRequired { current_cursor });
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

    let applied = apply_peer_delta_operations(db, peer_id, &operations)
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
///   `(peer_id, remote_book_id)`, storing the owner's `added_at` so the
///   "new" badge reflects the origin library, not local observation time.
///   `notified_at` is preserved on existing rows.
/// - `{ "op": "delete", "book_id": N }` removes the row for
///   `(peer_id, N)`; absent rows are silent no-ops (idempotent).
///
/// Returns the number of operations that made it past basic shape
/// validation. Malformed ops are logged and skipped without aborting.
pub async fn apply_peer_delta_operations(
    db: &DatabaseConnection,
    peer_id: i32,
    operations: &[serde_json::Value],
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

                upsert_peer_book_row(db, peer_id, remote_id, &book, &now).await?;
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
        if book.added_at.is_some() {
            active.added_at = Set(book.added_at.clone());
        }
        // Mirror `upsert_peer_books_cache` (HTTP path): the owner is the
        // source of truth for loan state. Persisting these lets the iPhone
        // carousel filter drop books that aren't borrowable.
        active.owned = Set(book.owned.unwrap_or(true));
        active.available_copies = Set(book.available_copies);
        // notified_at intentionally preserved.
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
            first_seen_at: Set(None),
            added_at: Set(book.added_at.clone()),
            notified_at: Set(None),
            owned: Set(book.owned.unwrap_or(true)),
            available_copies: Set(book.available_copies),
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

/// Public wrapper around [`persist_peer_cursor`] exposed to the FFI layer.
///
/// Intended use: after a successful legacy `library_manifest_request` +
/// page loop triggered by a `ResetRequired { current_cursor: Some(N) }`
/// outcome, the Flutter side calls this with `N` so the next sync resumes
/// as a delta instead of re-entering the reset loop.
///
/// Not called from the delta orchestrator itself, which still writes the
/// cursor via the private helper on its own success path.
pub async fn set_peer_last_delta_cursor(
    db: &DatabaseConnection,
    peer_id: i32,
    cursor: i64,
) -> Result<(), sea_orm::DbErr> {
    persist_peer_cursor(db, peer_id, cursor).await
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
        upsert_op_with_added_at(remote_book_id, title, Some("2026-04-15T10:00:00+00:00"))
    }

    fn upsert_op_with_added_at(
        remote_book_id: i32,
        title: &str,
        added_at: Option<&str>,
    ) -> serde_json::Value {
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
                "added_at": added_at,
            }
        })
    }

    #[tokio::test]
    async fn apply_upsert_inserts_new_row() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        let ops = vec![upsert_op(42, "Hello World")];
        let applied = apply_peer_delta_operations(&db, peer_id, &ops)
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
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-04-15T10:00:00+00:00"),
            "added_at must be stored from the owner's payload",
        );
    }

    #[tokio::test]
    async fn upsert_refreshes_added_at_from_owner() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        apply_peer_delta_operations(
            &db,
            peer_id,
            &[upsert_op_with_added_at(
                10,
                "Original",
                Some("2026-01-01T00:00:00+00:00"),
            )],
        )
        .await
        .unwrap();

        apply_peer_delta_operations(
            &db,
            peer_id,
            &[upsert_op_with_added_at(
                10,
                "Renamed",
                Some("2026-03-01T00:00:00+00:00"),
            )],
        )
        .await
        .unwrap();

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(10))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.title, "Renamed");
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-03-01T00:00:00+00:00"),
            "owner's added_at is the source of truth on every upsert",
        );
    }

    #[tokio::test]
    async fn upsert_without_added_at_preserves_existing() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        apply_peer_delta_operations(
            &db,
            peer_id,
            &[upsert_op_with_added_at(
                11,
                "Original",
                Some("2026-01-01T00:00:00+00:00"),
            )],
        )
        .await
        .unwrap();

        // Older peer (no added_at) pushes an update: preserve the value.
        apply_peer_delta_operations(&db, peer_id, &[upsert_op_with_added_at(11, "Edited", None)])
            .await
            .unwrap();

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(11))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-01-01T00:00:00+00:00"),
            "missing added_at must not overwrite a known value",
        );
    }

    #[tokio::test]
    async fn apply_delete_removes_row() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        apply_peer_delta_operations(&db, peer_id, &[upsert_op(7, "Doomed")])
            .await
            .unwrap();

        let ops = vec![json!({ "op": "delete", "book_id": 7 })];
        let applied = apply_peer_delta_operations(&db, peer_id, &ops)
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
        let applied = apply_peer_delta_operations(&db, peer_id, &ops)
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
        let applied = apply_peer_delta_operations(&db, peer_id, &ops)
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

    /// Loan status from the owner (owned=false, available_copies=Some(0))
    /// must round-trip through the relay delta path — same guarantee as the
    /// HTTP `upsert_peer_books_cache` path, otherwise the iPhone-side
    /// carousel filter can't hide fully-lent books when sync is via relay.
    #[tokio::test]
    async fn upsert_persists_loan_status_on_insert_and_update() {
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        // INSERT path: owner lent out every copy of book 20.
        let fully_lent = json!({
            "op": "upsert",
            "book": {
                "id": 20,
                "title": "All lent out",
                "owned": true,
                "available_copies": 0,
                "added_at": "2026-04-15T10:00:00+00:00",
            }
        });
        // INSERT path: owner borrowed book 21 from someone else.
        let peer_borrowed = json!({
            "op": "upsert",
            "book": {
                "id": 21,
                "title": "I borrowed it",
                "owned": false,
                "available_copies": 1,
                "added_at": "2026-04-15T10:00:00+00:00",
            }
        });
        apply_peer_delta_operations(&db, peer_id, &[fully_lent, peer_borrowed])
            .await
            .unwrap();

        let fetch = |remote_id: i32| {
            let db = db.clone();
            async move {
                peer_book::Entity::find()
                    .filter(peer_book::Column::PeerId.eq(peer_id))
                    .filter(peer_book::Column::RemoteBookId.eq(remote_id))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let row_lent = fetch(20).await;
        let row_borrowed = fetch(21).await;
        assert!(row_lent.owned);
        assert_eq!(row_lent.available_copies, Some(0));
        assert!(!row_borrowed.owned);
        assert_eq!(row_borrowed.available_copies, Some(1));

        // UPDATE path: a later sync reports book 20 now has one copy back
        // and book 21 has been returned to its owner (so we no longer hold
        // a temporary copy, i.e. owner flips back to true).
        let lent_updated = json!({
            "op": "upsert",
            "book": {
                "id": 20,
                "title": "All lent out",
                "owned": true,
                "available_copies": 2,
                "added_at": "2026-04-15T10:00:00+00:00",
            }
        });
        apply_peer_delta_operations(&db, peer_id, &[lent_updated])
            .await
            .unwrap();
        let refreshed = fetch(20).await;
        assert_eq!(
            refreshed.available_copies,
            Some(2),
            "update must refresh available_copies (otherwise the cache stays stale)",
        );
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

    #[tokio::test]
    async fn set_peer_last_delta_cursor_is_idempotent() {
        // Public helper used by Flutter after a successful legacy full sync
        // triggered by a `reset_required:<N>` outcome. Writing the same
        // value twice must be a no-op (idempotent retry).
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        set_peer_last_delta_cursor(&db, peer_id, 999)
            .await
            .expect("first write succeeds");
        set_peer_last_delta_cursor(&db, peer_id, 999)
            .await
            .expect("second write succeeds (idempotent)");

        let reloaded = peer::Entity::find_by_id(peer_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.last_delta_cursor, Some(999));
    }

    #[tokio::test]
    async fn set_peer_last_delta_cursor_overwrites_existing() {
        // When a responder reports a newer `current_cursor` than the one
        // already persisted, the helper must overwrite unconditionally —
        // the delta orchestrator is the only path that advances the cursor
        // incrementally; the reset-recovery path always jumps forward.
        let db = setup().await;
        let peer_id = create_peer(&db).await;

        persist_peer_cursor(&db, peer_id, 100).await.unwrap();
        set_peer_last_delta_cursor(&db, peer_id, 500).await.unwrap();

        let reloaded = peer::Entity::find_by_id(peer_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.last_delta_cursor, Some(500));
    }
}
