//! Borrower-initiated book returns.

use super::*;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct ReturnBorrowedBookPayload {
    pub copy_id: String,
}

/// Outcome of a borrower-initiated return.
///
/// The local copy is removed on every path, so HTTP 200 says nothing useful on
/// its own. `lender_notified` is what the UI must branch on: a return the lender
/// never hears about leaves the book out on loan on their side, indefinitely,
/// with no signal anywhere. Reporting that as a success is a lie, and the user
/// loses the only moment they could have acted on it.
///
/// `reason` names the failure so the UI (and a bug report) can tell the cases
/// apart. It is None exactly when `lender_notified` is true.
pub(crate) fn return_outcome(
    lender_notified: bool,
    reason: Option<&str>,
) -> axum::response::Response {
    let message = if lender_notified {
        "Book returned successfully"
    } else {
        "Copy removed locally; the lender was not notified"
    };
    (
        StatusCode::OK,
        Json(json!({
            "message": message,
            "lender_notified": lender_notified,
            "reason": reason,
        })),
    )
        .into_response()
}

/// Borrower initiates a return: notifies the lender and cleans up local data.
pub async fn return_borrowed_book(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<ReturnBorrowedBookPayload>,
) -> impl IntoResponse {
    use crate::models::{book, copy, p2p_outgoing_request, peer};
    let db = state.db().clone();

    tracing::info!(
        "📚 Borrower initiating return for copy_id: {}",
        payload.copy_id
    );

    // 1. Look up the copy to get book_id, then the book to get ISBN
    let the_copy = match copy::Entity::find_by_id(payload.copy_id.clone())
        .one(&db)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Copy not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let book_isbn = match book::Entity::find_by_id(the_copy.book_id.clone())
        .one(&db)
        .await
    {
        Ok(Some(b)) => b.isbn.unwrap_or_default(),
        _ => String::new(),
    };

    // 2. Find the outgoing request for this loan
    let outgoing = find_accepted_outgoing_request(&db, &the_copy, &book_isbn).await;

    let outgoing_req = match outgoing {
        Ok(Some(req)) => req,
        Ok(None) => {
            // Reached on a second synced device: `copies` replicates across the
            // account, `p2p_outgoing_request` does not, so the request that names
            // the loan simply is not here. The copy goes, the lender never knows.
            tracing::warn!(
                "No accepted outgoing request found for ISBN: '{}'. Falling back to local cleanup.",
                book_isbn
            );
            // Fallback: delete the local copy + clean up orphaned book
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            retain_returned_book(&db, the_copy.book_id).await;
            return return_outcome(false, Some("no_outgoing_request"));
        }
        Err(e) => {
            tracing::error!("DB error finding outgoing request: {}", e);
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            retain_returned_book(&db, the_copy.book_id).await;
            return return_outcome(false, Some("request_lookup_failed"));
        }
    };

    // 2. Find the peer (lender)
    let peer = match peer::Entity::find_by_id(outgoing_req.to_peer_id)
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::warn!("Peer not found for outgoing request");
            // Still clean up locally
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            retain_returned_book(&db, the_copy.book_id).await;
            let mut active: p2p_outgoing_request::ActiveModel = outgoing_req.into();
            active.status = Set("returned".to_string());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(&db).await;
            return return_outcome(false, Some("peer_unknown"));
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 3. Notify the lender to mark the loan as returned.
    //
    // Awaited, both on the E2EE path and on the plaintext fallback. The fallback
    // used to be spawned, so its failure only ever reached the log while the user
    // was shown a success: the one moment they could have acted on it. The E2EE
    // attempt above already blocks, so awaiting the fallback adds no wait that was
    // not already there.
    let lender_request_id = outgoing_req.lender_request_id.clone();
    let mut reason: Option<&str> = None;
    let lender_notified = if let Some(ref lender_req_id) = lender_request_id {
        let return_payload = json!({
            "loan_id": lender_req_id,
            "status": "returned",
        });

        // Try E2EE first
        match try_send_e2ee(&state, &peer, "status_update", return_payload).await {
            Ok(Some(_)) => {
                tracing::info!(
                    "E2EE: Return notification sent to {} (encrypted)",
                    peer.name
                );
                true
            }
            Err(e) => {
                // No plaintext retry here, as before: an E2EE channel that errors
                // is not the same as one that is absent.
                tracing::warn!("E2EE: Return notification error: {e}");
                reason = Some("e2ee_send_failed");
                false
            }
            Ok(None) => {
                // Plaintext fallback
                let url = format!("{}/api/peers/requests/{}", peer.url, lender_req_id);
                match get_safe_client()
                    .put(&url)
                    .json(&serde_json::json!({ "status": "returned" }))
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                {
                    Ok(res) if res.status().is_success() => {
                        tracing::info!("Return notification sent to lender: {}", res.status());
                        true
                    }
                    Ok(res) => {
                        tracing::warn!("Lender refused the return notification: {}", res.status());
                        reason = Some("lender_refused_notification");
                        false
                    }
                    Err(e) => {
                        tracing::warn!("Failed to send return notification to lender: {}", e);
                        reason = Some("lender_unreachable");
                        false
                    }
                }
            }
        }
    } else {
        tracing::warn!(
            "No lender_request_id on outgoing request — cannot notify lender. \
             Lender will need to mark the return manually."
        );
        reason = Some("no_lender_request_id");
        false
    };

    // 4. Update outgoing request status to "returned"
    let mut active: p2p_outgoing_request::ActiveModel = outgoing_req.into();
    active.status = Set("returned".to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    if let Err(e) = active.update(&db).await {
        tracing::warn!("Failed to update outgoing request status: {}", e);
    }

    // 5. Delete the borrowed copy
    if let Err(e) = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await {
        tracing::warn!("Failed to delete borrowed copy: {}", e);
    }

    // 6. Clean up book if no longer needed
    retain_returned_book(&db, the_copy.book_id).await;

    return_outcome(lender_notified, reason)
}

/// Find the accepted outgoing request matching the borrowed copy being returned.
///
/// `book_id` identifies the loan, and the lender recorded on the copy narrows it
/// further: a book row is shared across ISBN-equal borrows, so two accepted
/// requests can name the same `book_id`. Requests written before that column
/// existed carry NULL and fall back to the ISBN, narrowed the same way. A bare
/// ISBN lookup is not enough on its own: a book lent without one is stored as
/// `""`, and a shared ISBN can name a different loan. Getting this wrong marks the
/// wrong loan returned and notifies the wrong peer.
pub(crate) async fn find_accepted_outgoing_request(
    db: &DatabaseConnection,
    the_copy: &crate::models::copy::Model,
    book_isbn: &str,
) -> Result<Option<crate::models::p2p_outgoing_request::Model>, sea_orm::DbErr> {
    use crate::models::p2p_outgoing_request;

    /// Restrict a query to the lender the copy was borrowed from, when known.
    fn scoped_to_lender(
        query: sea_orm::Select<p2p_outgoing_request::Entity>,
        lender_peer_id: Option<i32>,
    ) -> sea_orm::Select<p2p_outgoing_request::Entity> {
        match lender_peer_id {
            Some(id) => query.filter(p2p_outgoing_request::Column::ToPeerId.eq(id)),
            None => query,
        }
    }

    /// Resolve the query to at most one request.
    ///
    /// When the copy names its lender, the query is already narrowed to that peer and
    /// the first match is taken, as before. When it does not, several accepted
    /// requests on the same book row are indistinguishable: `.one()` would return
    /// whichever SQLite yields first, closing an arbitrary loan and notifying a peer
    /// who never lent the book. Declining leaves a stale copy the user can remove,
    /// which is recoverable; notifying the wrong lender is not.
    async fn resolve(
        db: &DatabaseConnection,
        query: sea_orm::Select<p2p_outgoing_request::Entity>,
        lender_peer_id: Option<i32>,
    ) -> Result<Option<p2p_outgoing_request::Model>, sea_orm::DbErr> {
        use sea_orm::QuerySelect;

        if lender_peer_id.is_some() {
            return query.one(db).await;
        }
        let mut found = query.limit(2).all(db).await?;
        if found.len() > 1 {
            tracing::warn!(
                "Return: several accepted requests match a copy with no lender, refusing to guess"
            );
            return Ok(None);
        }
        Ok(found.pop())
    }

    let exact = resolve(
        db,
        scoped_to_lender(
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookId.eq(the_copy.book_id.as_str()))
                .filter(p2p_outgoing_request::Column::Status.eq("accepted")),
            the_copy.lender_peer_id,
        ),
        the_copy.lender_peer_id,
    )
    .await;

    match exact {
        Ok(Some(req)) => Ok(Some(req)),
        Ok(None) if !book_isbn.is_empty() => {
            resolve(
                db,
                scoped_to_lender(
                    p2p_outgoing_request::Entity::find()
                        .filter(p2p_outgoing_request::Column::BookIsbn.eq(book_isbn))
                        .filter(p2p_outgoing_request::Column::Status.eq("accepted")),
                    the_copy.lender_peer_id,
                ),
                the_copy.lender_peer_id,
            )
            .await
        }
        other => other,
    }
}

/// Record that a returned book stays in the library.
///
/// Returning a borrowed copy removes the copy, never the book. A book the user
/// read without owning it is a first-class state (`owned = false` combined with
/// any `reading_status`), and it carries reading dates, a rating and notes that
/// the reader entered. Deleting it would destroy that, so removing a book from
/// the library is left to an explicit user action.
///
/// `owned` is left untouched for the same reason: `create_borrowed_copy` reuses
/// an existing book row matched by ISBN, so a reclaimed loan can hang off a book
/// the user genuinely owns. Forcing `owned = false` here would un-own it.
pub(crate) async fn retain_returned_book(db: &DatabaseConnection, book_id: String) {
    use crate::models::{book, copy};
    if let Ok(Some(bk)) = book::Entity::find_by_id(book_id).one(db).await {
        let copy_count = copy::Entity::find()
            .filter(copy::Column::BookId.eq(bk.id.as_str()))
            .count(db)
            .await
            .unwrap_or(0);

        tracing::info!(
            "Book {} kept after loan return (owned={}, reading_status='{}', copies={})",
            bk.id,
            bk.owned,
            bk.reading_status,
            copy_count
        );
    }
}

/// Returning a borrowed copy removes the copy, never the book.
#[cfg(test)]
mod retain_returned_book_tests {
    use super::*;
    use crate::db;
    use crate::models::{book, copy};
    use sea_orm::Set;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_book(db: &DatabaseConnection, title: &str, isbn: Option<&str>) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        book::ActiveModel {
            title: Set(title.to_string()),
            isbn: Set(isbn.map(|s| s.to_string())),
            owned: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert book")
        .id
    }

    async fn insert_test_peer(db: &DatabaseConnection, name: &str) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        crate::models::peer::ActiveModel {
            name: Set(name.to_string()),
            url: Set(format!("http://{name}.local:8000")),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer")
        .id
    }

    async fn insert_borrowed_copy(
        db: &DatabaseConnection,
        book_id: &str,
        lender_peer_id: Option<i32>,
    ) -> copy::Model {
        let lib_id = crate::utils::library_helpers::resolve_library_id(db)
            .await
            .expect("library");
        let now = chrono::Utc::now().to_rfc3339();
        copy::ActiveModel {
            book_id: Set(book_id.to_string()),
            library_id: Set(lib_id),
            status: Set("borrowed".to_string()),
            is_temporary: Set(true),
            lender_peer_id: Set(lender_peer_id),
            borrow_source: Set(Some("peer".to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert copy")
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_request(
        db: &DatabaseConnection,
        id: &str,
        to_peer_id: i32,
        isbn: &str,
        book_id: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        crate::models::p2p_outgoing_request::ActiveModel {
            id: Set(id.to_string()),
            to_peer_id: Set(to_peer_id),
            book_isbn: Set(isbn.to_string()),
            book_title: Set("Le Livre".to_string()),
            status: Set("accepted".to_string()),
            lender_request_id: Set(None),
            book_id: Set(book_id.map(|s| s.to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .expect("insert request");
    }

    /// Two peers lent books sharing an ISBN. Resolving the loan by ISBN alone picks
    /// an arbitrary row, which marks the wrong loan returned and notifies the wrong
    /// lender. `book_id` names it exactly.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_returned_loan_is_resolved_by_book_id_not_by_isbn() {
        let db = setup().await;
        let alice = insert_test_peer(&db, "alice").await;
        let bob = insert_test_peer(&db, "bob").await;

        // Alice's request is inserted first: a bare ISBN lookup would return it.
        let alice_book = insert_book(&db, "Chez Alice", Some("978-same")).await;
        insert_request(&db, "loan-alice", alice, "978-same", Some(&alice_book)).await;
        let bob_book = insert_book(&db, "Chez Bob", Some("978-same")).await;
        insert_request(&db, "loan-bob", bob, "978-same", Some(&bob_book)).await;

        let bob_copy = insert_borrowed_copy(&db, &bob_book, Some(bob)).await;
        let found = find_accepted_outgoing_request(&db, &bob_copy, "978-same")
            .await
            .expect("query");

        assert_eq!(
            found.map(|r| r.id),
            Some("loan-bob".to_string()),
            "returning Bob's copy must resolve Bob's loan"
        );
    }

    /// A book row is shared across ISBN-equal borrows, so two accepted requests can
    /// carry the same `book_id`. The lender recorded on the copy is what tells them
    /// apart: without it, returning Bob's copy would close Alice's loan and notify
    /// her instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_loans_of_the_same_book_row_are_told_apart_by_the_lender() {
        let db = setup().await;
        let alice = insert_test_peer(&db, "alice").await;
        let bob = insert_test_peer(&db, "bob").await;

        // One shared book row, two accepted loans. Alice's request is inserted first,
        // so an unscoped `.one()` would return it.
        let book_id = insert_book(&db, "Le Livre", Some("978-same")).await;
        insert_request(&db, "loan-alice", alice, "978-same", Some(&book_id)).await;
        insert_request(&db, "loan-bob", bob, "978-same", Some(&book_id)).await;

        let bob_copy = insert_borrowed_copy(&db, &book_id, Some(bob)).await;
        let found = find_accepted_outgoing_request(&db, &bob_copy, "978-same")
            .await
            .expect("query");

        assert_eq!(
            found.map(|r| r.id),
            Some("loan-bob".to_string()),
            "returning Bob's copy must close Bob's loan, not Alice's"
        );
    }

    /// A copy whose lender is unknown cannot pick between two accepted requests on
    /// the same book row. Guessing marks the wrong loan returned and notifies the
    /// wrong peer, so the lookup declines. A single candidate still resolves, which
    /// is what keeps legacy copies returnable.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unidentified_copy_refuses_to_guess_between_two_loans() {
        let db = setup().await;
        let alice = insert_test_peer(&db, "alice").await;
        let bob = insert_test_peer(&db, "bob").await;

        let book_id = insert_book(&db, "Le Livre", Some("978-same")).await;
        insert_request(&db, "loan-alice", alice, "978-same", Some(&book_id)).await;
        insert_request(&db, "loan-bob", bob, "978-same", Some(&book_id)).await;

        let orphan_copy = insert_borrowed_copy(&db, &book_id, None).await;
        let found = find_accepted_outgoing_request(&db, &orphan_copy, "978-same")
            .await
            .expect("query");

        assert!(
            found.is_none(),
            "an ambiguous return must notify nobody rather than the wrong peer"
        );
    }

    /// The counterpart: one candidate, no ambiguity, the legacy copy still resolves.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unidentified_copy_still_resolves_a_single_loan() {
        let db = setup().await;
        let alice = insert_test_peer(&db, "alice").await;

        let book_id = insert_book(&db, "Le Livre", Some("978-same")).await;
        insert_request(&db, "loan-alice", alice, "978-same", Some(&book_id)).await;

        let orphan_copy = insert_borrowed_copy(&db, &book_id, None).await;
        let found = find_accepted_outgoing_request(&db, &orphan_copy, "978-same")
            .await
            .expect("query");

        assert_eq!(found.map(|r| r.id), Some("loan-alice".to_string()));
    }

    /// Rows predating the `book_id` column fall back to the ISBN, narrowed to the
    /// lender the copy records.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_request_falls_back_to_the_isbn_scoped_to_the_lender() {
        let db = setup().await;
        let alice = insert_test_peer(&db, "alice").await;
        let bob = insert_test_peer(&db, "bob").await;

        insert_request(&db, "loan-alice", alice, "978-same", None).await;
        insert_request(&db, "loan-bob", bob, "978-same", None).await;

        let book_id = insert_book(&db, "Le Livre", Some("978-same")).await;
        let bob_copy = insert_borrowed_copy(&db, &book_id, Some(bob)).await;
        let found = find_accepted_outgoing_request(&db, &bob_copy, "978-same")
            .await
            .expect("query");

        assert_eq!(
            found.map(|r| r.id),
            Some("loan-bob".to_string()),
            "the lender on the copy disambiguates the legacy rows"
        );
    }

    /// A book lent without an ISBN used to be unresolvable, so the lender was never
    /// notified of the return. `book_id` fixes that.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_loan_without_an_isbn_is_still_resolved() {
        let db = setup().await;
        let alice = insert_test_peer(&db, "alice").await;
        let book_id = insert_book(&db, "Sans ISBN", None).await;
        insert_request(&db, "loan-alice", alice, "", Some(&book_id)).await;

        let the_copy = insert_borrowed_copy(&db, &book_id, Some(alice)).await;
        let found = find_accepted_outgoing_request(&db, &the_copy, "")
            .await
            .expect("query");

        assert_eq!(found.map(|r| r.id), Some("loan-alice".to_string()));
    }

    /// The nominal losing scenario before this fix: borrow a book from a peer,
    /// read it, mark it read, give it back. The book used to be deleted along
    /// with the reading dates, the rating and the notes.
    #[tokio::test(flavor = "multi_thread")]
    async fn returning_a_read_but_unowned_book_keeps_it() {
        let db = setup().await;
        let now = chrono::Utc::now().to_rfc3339();
        let bk = book::ActiveModel {
            title: Set("Le Livre".to_string()),
            owned: Set(false),
            reading_status: Set("read".to_string()),
            user_rating: Set(Some(8)),
            finished_reading_at: Set(Some("2026-07-01".to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert book");

        // The handler deletes the borrowed copy before calling us.
        retain_returned_book(&db, bk.id.clone()).await;

        let after = book::Entity::find_by_id(bk.id.as_str())
            .one(&db)
            .await
            .expect("find")
            .expect("book must survive a loan return");
        assert!(!after.owned);
        assert_eq!(after.reading_status, "read");
        assert_eq!(after.user_rating, Some(8));
        assert_eq!(after.finished_reading_at.as_deref(), Some("2026-07-01"));
    }

    /// A wishlist book with no copies is equally retained: nothing about the
    /// return path may delete a book row.
    #[tokio::test(flavor = "multi_thread")]
    async fn returning_never_deletes_even_a_bare_book() {
        let db = setup().await;
        let now = chrono::Utc::now().to_rfc3339();
        let bk = book::ActiveModel {
            title: Set("Nu".to_string()),
            owned: Set(false),
            reading_status: Set("to_read".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert book");

        retain_returned_book(&db, bk.id.clone()).await;

        assert!(
            book::Entity::find_by_id(bk.id.as_str())
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "a to_read, unowned, copy-less book must still be retained"
        );
        assert_eq!(
            copy::Entity::find()
                .filter(copy::Column::BookId.eq(bk.id.as_str()))
                .count(&db)
                .await
                .expect("count"),
            0
        );
    }
}
