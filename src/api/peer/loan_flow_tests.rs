//! Cross-concern integration tests of the P2P loan flow.
//!
//! The inner test modules pull their scope from this header via `use super::*`,
//! exactly as they did at the bottom of the former single-file `peer.rs`.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, QueryFilter};
use serde_json::json;

/// The plaintext loan-offer endpoint must resolve the lender it is talking to.
///
/// `p2p_outgoing_requests.to_peer_id` carries a foreign key to `peers(id)`, whose
/// autoincrement starts at 1, so the `0` sentinel this handler used to write could
/// never satisfy it: the INSERT failed on every call that a `foreign_keys` pragma
/// witnessed, and the return flow found no request to notify the lender through.
/// These tests run on `init_db`, which enforces the pragma, so the sentinel is a
/// hard failure rather than a silent one.
#[cfg(test)]
mod loan_offer_lender_resolution_tests {
    use super::*;
    use crate::db;
    use crate::models::{copy, p2p_outgoing_request};
    use sea_orm::{EntityTrait, PaginatorTrait, Set};

    const LENDER_UUID: &str = "6f1d1a4e-0000-4000-8000-000000000001";

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    /// A peer we have already paired with, carrying the stable `library_uuid`
    /// the offer payload identifies itself by.
    async fn insert_known_lender(db: &DatabaseConnection, library_uuid: Option<&str>) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set("christophe".to_string()),
            url: Set("http://christophe.local:8000".to_string()),
            library_uuid: Set(library_uuid.map(|s| s.to_string())),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer")
        .id
    }

    fn offer(library_uuid: Option<&str>) -> LoanOffer {
        LoanOffer {
            isbn: Some("978-1".to_string()),
            title: "Le Livre".to_string(),
            author: Some("Jack London".to_string()),
            cover_url: None,
            lender_name: "christophe".to_string(),
            due_date: "2026-09-01".to_string(),
            request_id: Some("lender-req-1".to_string()),
            library_uuid: library_uuid.map(|s| s.to_string()),
        }
    }

    fn acceptance_payload() -> serde_json::Value {
        json!({
            "title": "Le Livre",
            "isbn": "978-1",
            "lender_name": "christophe",
            "due_date": "2026-09-01",
        })
    }

    async fn insert_pending_request(db: &DatabaseConnection, id: &str, to_peer_id: i32) {
        let now = chrono::Utc::now().to_rfc3339();
        p2p_outgoing_request::ActiveModel {
            id: Set(id.to_string()),
            to_peer_id: Set(to_peer_id),
            book_isbn: Set("978-1".to_string()),
            book_title: Set("Le Livre".to_string()),
            status: Set("pending".to_string()),
            lender_request_id: Set(None),
            book_id: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .expect("insert outgoing request");
    }

    async fn borrowed_copy(db: &DatabaseConnection) -> copy::Model {
        copy::Entity::find()
            .filter(copy::Column::Status.eq("borrowed"))
            .one(db)
            .await
            .expect("query copies")
            .expect("a borrowed copy exists")
    }

    /// Acceptance criterion: an offer from a known peer records an outgoing
    /// request pointing at that peer, and stamps the copy with the same lender.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offer_from_a_known_peer_records_its_outgoing_request() {
        let db = setup_db().await;
        let lender = insert_known_lender(&db, Some(LENDER_UUID)).await;

        let response = receive_loan_offer(State(db.clone()), Json(offer(Some(LENDER_UUID))))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let request = p2p_outgoing_request::Entity::find()
            .one(&db)
            .await
            .expect("query outgoing requests")
            .expect("the offer records an outgoing request");
        assert_eq!(
            request.to_peer_id, lender,
            "the outgoing request must point at the real lender, not a sentinel"
        );
        assert_eq!(request.status, "accepted");
        assert_eq!(request.lender_request_id.as_deref(), Some("lender-req-1"));

        assert_eq!(
            borrowed_copy(&db).await.lender_peer_id,
            Some(lender),
            "the borrowed copy must carry the lender it came from"
        );
    }

    /// The return endpoint answers 200 whether or not the lender heard about it,
    /// because the local copy is removed either way. `lender_notified` is the only
    /// field that tells the two apart, and a silent return leaves the book out on
    /// loan on the lender's side forever. Lock the contract the UI branches on.
    #[test]
    fn a_return_that_notified_nobody_does_not_claim_success() {
        use axum::body::to_bytes;

        async fn body_of(response: axum::response::Response) -> serde_json::Value {
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let silent = rt.block_on(body_of(return_outcome(false, Some("no_outgoing_request"))));
        assert_eq!(silent["lender_notified"], serde_json::json!(false));
        assert_eq!(silent["reason"], serde_json::json!("no_outgoing_request"));
        assert!(
            !silent["message"].as_str().unwrap().contains("successfully"),
            "a return nobody heard must not read as a success: {}",
            silent["message"]
        );

        let notified = rt.block_on(body_of(return_outcome(true, None)));
        assert_eq!(notified["lender_notified"], serde_json::json!(true));
        assert_eq!(
            notified["reason"],
            serde_json::Value::Null,
            "reason is None exactly when the lender was notified"
        );
    }

    /// Acceptance criterion: returning the book finds the request that notifies
    /// the lender. `find_accepted_outgoing_request` scopes on the copy's
    /// `lender_peer_id`, so both columns have to agree for the return to resolve.
    #[tokio::test(flavor = "multi_thread")]
    async fn returning_a_plaintext_offered_book_finds_the_lender_to_notify() {
        let db = setup_db().await;
        let lender = insert_known_lender(&db, Some(LENDER_UUID)).await;

        let _ = receive_loan_offer(State(db.clone()), Json(offer(Some(LENDER_UUID))))
            .await
            .into_response();

        let the_copy = borrowed_copy(&db).await;
        let request = find_accepted_outgoing_request(&db, &the_copy, "978-1")
            .await
            .expect("query")
            .expect("the return flow finds the request that names the lender");
        assert_eq!(request.to_peer_id, lender);
        assert_eq!(request.lender_request_id.as_deref(), Some("lender-req-1"));
    }

    /// Acceptance criterion: an offer from a sender that carries no identity
    /// still creates the borrowed copy. It records no outgoing request, because
    /// there is no peer to point one at, and says so in the log.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offer_without_peer_identity_still_creates_the_copy() {
        let db = setup_db().await;
        insert_known_lender(&db, Some(LENDER_UUID)).await;

        let response = receive_loan_offer(State(db.clone()), Json(offer(None)))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(
            borrowed_copy(&db).await.lender_peer_id,
            None,
            "an unidentified lender leaves the back-reference empty"
        );
        assert_eq!(
            p2p_outgoing_request::Entity::find()
                .count(&db)
                .await
                .expect("count"),
            0,
            "no outgoing request may be forged for an unresolvable lender"
        );
    }

    /// An offer naming a `library_uuid` we have never paired with resolves to no
    /// peer. Same degraded path as a sender that omits the field entirely: the
    /// copy is created, no request is forged against an arbitrary peer row.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_offer_from_an_unknown_library_uuid_forges_no_request() {
        let db = setup_db().await;
        insert_known_lender(&db, Some(LENDER_UUID)).await;

        let response = receive_loan_offer(
            State(db.clone()),
            Json(offer(Some("6f1d1a4e-0000-4000-8000-00000000dead"))),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(borrowed_copy(&db).await.lender_peer_id, None);
        assert_eq!(
            p2p_outgoing_request::Entity::find()
                .count(&db)
                .await
                .expect("count"),
            0
        );
    }

    /// A confirmation carrying neither `requester_request_id` nor an ISBN matches no
    /// outgoing request. The empty string must never be used as a search key: every
    /// request for a book without an ISBN stores `book_isbn = ""`, so `eq("")` would
    /// name an unrelated peer as this lender, and `lender_peer_id` scopes the reclaim
    /// purge. Best-effort acceptance of the copy, never a guessed lender.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_confirmation_without_isbn_or_request_id_names_no_lender() {
        let db = setup_db().await;
        let bystander = insert_known_lender(&db, Some(LENDER_UUID)).await;

        // A pending loan of a book that has no ISBN, granted by an unrelated peer.
        let now = chrono::Utc::now().to_rfc3339();
        p2p_outgoing_request::ActiveModel {
            id: Set("unrelated-req".to_string()),
            to_peer_id: Set(bystander),
            book_isbn: Set(String::new()),
            book_title: Set("Un Autre Livre".to_string()),
            status: Set("pending".to_string()),
            lender_request_id: Set(None),
            book_id: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("insert unrelated request");

        let payload = LoanConfirmation {
            isbn: None,
            title: "Le Livre".to_string(),
            author: None,
            cover_url: None,
            lender_name: "christophe".to_string(),
            due_date: "2026-09-01".to_string(),
            request_id: Some("lender-req-1".to_string()),
            requester_request_id: None,
        };
        let response = receive_loan_confirmation(State(db.clone()), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(
            borrowed_copy(&db).await.lender_peer_id,
            None,
            "an empty ISBN must not borrow an unrelated loan's lender"
        );

        let unrelated = p2p_outgoing_request::Entity::find_by_id("unrelated-req")
            .one(&db)
            .await
            .expect("query")
            .expect("the unrelated request survives");
        assert_eq!(
            unrelated.status, "pending",
            "the unrelated loan must not be flipped to accepted"
        );
        assert_eq!(unrelated.book_id, None);
    }

    /// The third plaintext writer reads its lender off the outgoing request it
    /// already loads to link `book_id`, so no payload field identifies the peer.
    #[tokio::test(flavor = "multi_thread")]
    async fn borrower_acceptance_stamps_the_lender_from_its_outgoing_request() {
        let db = setup_db().await;
        let lender = insert_known_lender(&db, Some(LENDER_UUID)).await;
        insert_pending_request(&db, "out-1", lender).await;

        process_borrower_acceptance(&db, "out-1", &acceptance_payload(), Some("lender-req-1"))
            .await;

        assert_eq!(
            borrowed_copy(&db).await.lender_peer_id,
            Some(lender),
            "the accepted borrow names its lender through the outgoing request"
        );
    }

    /// An acceptance whose outgoing request has vanished still creates the copy,
    /// with no lender back-reference, and says so in the log.
    #[tokio::test(flavor = "multi_thread")]
    async fn borrower_acceptance_without_an_outgoing_request_names_no_lender() {
        let db = setup_db().await;
        insert_known_lender(&db, Some(LENDER_UUID)).await;

        process_borrower_acceptance(&db, "missing", &acceptance_payload(), None).await;

        assert_eq!(borrowed_copy(&db).await.lender_peer_id, None);
    }

    /// The other plaintext writer resolves its lender from the outgoing request
    /// it already matched on, so no payload field is needed to identify the peer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_loan_confirmation_stamps_the_lender_from_its_outgoing_request() {
        let db = setup_db().await;
        let lender = insert_known_lender(&db, Some(LENDER_UUID)).await;

        insert_pending_request(&db, "borrower-req-1", lender).await;

        let payload = LoanConfirmation {
            isbn: Some("978-1".to_string()),
            title: "Le Livre".to_string(),
            author: None,
            cover_url: None,
            lender_name: "christophe".to_string(),
            due_date: "2026-09-01".to_string(),
            request_id: Some("lender-req-1".to_string()),
            requester_request_id: Some("borrower-req-1".to_string()),
        };
        let response = receive_loan_confirmation(State(db.clone()), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(
            borrowed_copy(&db).await.lender_peer_id,
            Some(lender),
            "the confirmation names its lender through the outgoing request"
        );
    }
}

/// A book row is a bibliographic record carrying many copies, and each copy can be
/// borrowed. The idempotency guard in `create_borrowed_copy` used to collapse that
/// to "at most one temporary borrowed copy per book row", which silently dropped a
/// second peer's loan of the same ISBN and let a copy borrowed from a contact block
/// a peer loan. The guard now keys on the lender, and scopes on `borrow_source`
/// rather than `is_temporary` so it agrees with the reclaim purge in `e2ee.rs`.
#[cfg(test)]
mod multi_lender_borrow_tests {
    use super::*;
    use crate::db;
    use crate::models::{copy, p2p_outgoing_request};
    use sea_orm::{EntityTrait, PaginatorTrait, Set};

    const ALICE_UUID: &str = "6f1d1a4e-0000-4000-8000-0000000000a1";
    const BOB_UUID: &str = "6f1d1a4e-0000-4000-8000-0000000000b0";

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer(db: &DatabaseConnection, name: &str, library_uuid: &str) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set(name.to_string()),
            url: Set(format!("http://{name}.local:8000")),
            library_uuid: Set(Some(library_uuid.to_string())),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer")
        .id
    }

    /// An offer of the same ISBN, from a named peer, with that peer's own request id.
    fn offer_from(library_uuid: &str, request_id: &str) -> LoanOffer {
        LoanOffer {
            isbn: Some("978-same".to_string()),
            title: "Le Livre".to_string(),
            author: None,
            cover_url: None,
            lender_name: "peer".to_string(),
            due_date: "2026-09-01".to_string(),
            request_id: Some(request_id.to_string()),
            library_uuid: Some(library_uuid.to_string()),
        }
    }

    async fn receive(db: &DatabaseConnection, offer: LoanOffer) {
        let response = receive_loan_offer(State(db.clone()), Json(offer))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn insert_book_with_isbn(
        db: &DatabaseConnection,
        title: &str,
        isbn: Option<&str>,
    ) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        crate::models::book::ActiveModel {
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

    /// A temporary peer-borrowed copy. `lender_peer_id: None` models a row written
    /// before that column existed.
    async fn insert_peer_copy(
        db: &DatabaseConnection,
        book_id: &str,
        lender_peer_id: Option<i32>,
    ) -> String {
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
        .id
    }

    /// An outgoing request written before the `book_id` column: only the ISBN links it
    /// to a local book row.
    async fn insert_legacy_request(db: &DatabaseConnection, id: &str, to_peer_id: i32, isbn: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        p2p_outgoing_request::ActiveModel {
            id: Set(id.to_string()),
            to_peer_id: Set(to_peer_id),
            book_isbn: Set(isbn.to_string()),
            book_title: Set("Le Livre".to_string()),
            status: Set("accepted".to_string()),
            lender_request_id: Set(None),
            book_id: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(db)
        .await
        .expect("insert legacy request");
    }

    /// Drive the plaintext reclaim endpoint the lender calls over the LAN.
    async fn mark_returned(db: &DatabaseConnection, request_id: &str, sender_uuid: &str) {
        // The lender now asserts its identity on the plaintext reclaim, and the borrower
        // requires it to name the loan's lender (ADR-050). These tests exercise the purge
        // logic, so they route through the gate with the real lender's uuid.
        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path(request_id.to_string()),
            Json(serde_json::json!({ "status": "returned", "library_uuid": sender_uuid })),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn borrowed_copies(db: &DatabaseConnection) -> Vec<copy::Model> {
        copy::Entity::find()
            .filter(copy::Column::Status.eq("borrowed"))
            .all(db)
            .await
            .expect("query copies")
    }

    /// Insert a copy borrowed from a contact, as the two Flutter writers do today.
    /// They disagree on `is_temporary`, so both shapes are exercised by callers.
    async fn insert_contact_copy(
        db: &DatabaseConnection,
        book_id: &str,
        is_temporary: bool,
        borrow_source: Option<&str>,
    ) -> String {
        let lib_id = crate::utils::library_helpers::resolve_library_id(db)
            .await
            .expect("library");
        let now = chrono::Utc::now().to_rfc3339();
        copy::ActiveModel {
            book_id: Set(book_id.to_string()),
            library_id: Set(lib_id),
            status: Set("borrowed".to_string()),
            is_temporary: Set(is_temporary),
            lender_display_name: Set(Some("Tante Jeanne".to_string())),
            lender_peer_id: Set(None),
            borrow_source: Set(borrow_source.map(|s| s.to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert contact copy")
        .id
    }

    /// Acceptance criterion: two peers lending the same ISBN produce two traceable
    /// loans. They share one book row, and each carries its own copy and its own
    /// outgoing request, so each return notifies the right lender.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_peers_lending_the_same_isbn_produce_two_loans() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let bob = insert_peer(&db, "bob", BOB_UUID).await;

        receive(&db, offer_from(ALICE_UUID, "alice-req")).await;
        receive(&db, offer_from(BOB_UUID, "bob-req")).await;

        let copies = borrowed_copies(&db).await;
        assert_eq!(copies.len(), 2, "each lender's loan gets its own copy");
        let mut lenders: Vec<Option<i32>> = copies.iter().map(|c| c.lender_peer_id).collect();
        lenders.sort();
        assert_eq!(lenders, vec![Some(alice), Some(bob)]);

        // One bibliographic record, as the app's model prescribes.
        let book_ids: std::collections::HashSet<_> =
            copies.iter().map(|c| c.book_id.clone()).collect();
        assert_eq!(book_ids.len(), 1, "both copies hang off one book row");

        let requests = p2p_outgoing_request::Entity::find()
            .all(&db)
            .await
            .expect("query requests");
        assert_eq!(
            requests.len(),
            2,
            "each loan is traceable on its own request"
        );
        let mut targets: Vec<i32> = requests.iter().map(|r| r.to_peer_id).collect();
        targets.sort();
        assert_eq!(targets, vec![alice, bob]);
    }

    /// Each of the two copies resolves the request that names its own lender, so the
    /// return notifies the right peer rather than closing the other's loan.
    #[tokio::test(flavor = "multi_thread")]
    async fn each_of_two_loans_returns_to_its_own_lender() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let bob = insert_peer(&db, "bob", BOB_UUID).await;

        receive(&db, offer_from(ALICE_UUID, "alice-req")).await;
        receive(&db, offer_from(BOB_UUID, "bob-req")).await;

        for (lender, expected_lender_request) in [(alice, "alice-req"), (bob, "bob-req")] {
            let the_copy = borrowed_copies(&db)
                .await
                .into_iter()
                .find(|c| c.lender_peer_id == Some(lender))
                .expect("a copy for this lender");
            let request = find_accepted_outgoing_request(&db, &the_copy, "978-same")
                .await
                .expect("query")
                .expect("the return finds this lender's request");
            assert_eq!(request.to_peer_id, lender);
            assert_eq!(
                request.lender_request_id.as_deref(),
                Some(expected_lender_request),
                "the return must reference the lender's own loan"
            );
        }
    }

    /// Acceptance criterion: a copy borrowed from a contact is never confused with a
    /// copy borrowed from a peer. It must not block a peer loan of the same book row.
    /// Both Flutter writers are covered: `book_details_screen` sends
    /// `is_temporary: false` with `borrow_source: 'contact'`, `borrow_book_screen`
    /// sends `is_temporary: true` and no `borrow_source` at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_contact_copy_does_not_block_a_peer_loan() {
        for (is_temporary, borrow_source) in [(false, Some("contact")), (true, None)] {
            let db = setup_db().await;
            let alice = insert_peer(&db, "alice", ALICE_UUID).await;

            let book_id = {
                let now = chrono::Utc::now().to_rfc3339();
                crate::models::book::ActiveModel {
                    title: Set("Le Livre".to_string()),
                    isbn: Set(Some("978-same".to_string())),
                    owned: Set(false),
                    created_at: Set(now.clone()),
                    updated_at: Set(now),
                    ..Default::default()
                }
                .insert(&db)
                .await
                .expect("insert book")
                .id
            };
            let contact_copy =
                insert_contact_copy(&db, &book_id, is_temporary, borrow_source).await;

            receive(&db, offer_from(ALICE_UUID, "alice-req")).await;

            let copies = borrowed_copies(&db).await;
            assert_eq!(
                copies.len(),
                2,
                "the peer loan is created alongside the contact copy (is_temporary={is_temporary}, borrow_source={borrow_source:?})"
            );
            assert!(
                copies.iter().any(|c| c.id == contact_copy),
                "the contact copy survives untouched"
            );
            assert!(
                copies.iter().any(|c| c.lender_peer_id == Some(alice)),
                "the peer loan records its lender"
            );
            assert_eq!(
                p2p_outgoing_request::Entity::find()
                    .count(&db)
                    .await
                    .expect("count"),
                1,
                "the peer loan is traceable"
            );
        }
    }

    /// The plaintext twin of `handle_status_update`. When a lender reclaims a book it
    /// must delete the copy IT lent, not whichever borrowed copy of the row SQLite
    /// happens to yield first. Bob's copy is inserted before Alice's on purpose: an
    /// unscoped `.one()` returns Bob's, so a reclaim by Alice would destroy Bob's
    /// live loan.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_reclaim_deletes_only_the_reclaiming_lenders_copy() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let bob = insert_peer(&db, "bob", BOB_UUID).await;

        receive(&db, offer_from(BOB_UUID, "bob-req")).await;
        receive(&db, offer_from(ALICE_UUID, "alice-req")).await;

        let copies = borrowed_copies(&db).await;
        assert_eq!(copies.len(), 2, "both loans exist before the reclaim");
        let bob_copy = copies
            .iter()
            .find(|c| c.lender_peer_id == Some(bob))
            .expect("bob's copy")
            .id
            .clone();
        let alice_copy = copies
            .iter()
            .find(|c| c.lender_peer_id == Some(alice))
            .expect("alice's copy")
            .id
            .clone();

        // Alice's own outgoing request, the one her reclaim names.
        let alice_request = p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::ToPeerId.eq(alice))
            .one(&db)
            .await
            .expect("query")
            .expect("alice's request");

        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path(alice_request.id.clone()),
            Json(serde_json::json!({ "status": "returned", "library_uuid": ALICE_UUID })),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert!(
            copy::Entity::find_by_id(alice_copy)
                .one(&db)
                .await
                .expect("find")
                .is_none(),
            "Alice reclaims the copy she lent"
        );
        assert!(
            copy::Entity::find_by_id(bob_copy)
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "Bob's loan is still running and must survive Alice's reclaim"
        );
    }

    /// An empty ISBN must behave exactly like an absent one, on every branch. Peers do
    /// lend books that carry no ISBN, so the title fallback is a live path: the loan
    /// attaches to the book row already holding that title rather than duplicating it.
    ///
    /// Both spellings a peer can send are exercised. Before the normalization, `Some("")`
    /// searched `Isbn.eq("")`, missed the NULL-ISBN row, and created a duplicate book.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_isbn_behaves_exactly_like_an_absent_one() {
        for sent_isbn in [None, Some(String::new())] {
            let db = setup_db().await;
            let alice = insert_peer(&db, "alice", ALICE_UUID).await;

            // The borrower already holds this title, catalogued without an ISBN.
            let existing = insert_book_with_isbn(&db, "Le Livre", None).await;

            let offer = LoanOffer {
                isbn: sent_isbn.clone(),
                title: "Le Livre".to_string(),
                author: None,
                cover_url: None,
                lender_name: "alice".to_string(),
                due_date: "2026-09-01".to_string(),
                request_id: Some("alice-req".to_string()),
                library_uuid: Some(ALICE_UUID.to_string()),
            };
            receive(&db, offer).await;

            let copies = borrowed_copies(&db).await;
            assert_eq!(
                copies.len(),
                1,
                "one borrowed copy (sent_isbn={sent_isbn:?})"
            );
            assert_eq!(
                copies[0].book_id, existing,
                "the loan joins the existing book row rather than duplicating it (sent_isbn={sent_isbn:?})"
            );
            assert_eq!(copies[0].lender_peer_id, Some(alice));

            assert_eq!(
                crate::models::book::Entity::find()
                    .count(&db)
                    .await
                    .expect("count"),
                1,
                "no duplicate book row is created (sent_isbn={sent_isbn:?})"
            );
        }
    }

    /// An empty ISBN is not an ISBN. `create_borrowed_copy` looks a book up by
    /// `Isbn.eq(isbn)`, so a payload carrying `"isbn": ""` matches any row whose ISBN is
    /// the empty string, and the borrowed copy lands on someone else's book. The same
    /// function then writes that empty string onto the books it creates, so the defect
    /// manufactures the rows it later collides with.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_isbn_never_matches_an_unrelated_book() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;

        // A book already carrying the empty-string ISBN, as the old writer produced.
        let unrelated = insert_book_with_isbn(&db, "Un Autre Livre", Some("")).await;

        let offer = LoanOffer {
            isbn: Some(String::new()),
            title: "Le Livre".to_string(),
            author: None,
            cover_url: None,
            lender_name: "alice".to_string(),
            due_date: "2026-09-01".to_string(),
            request_id: Some("alice-req".to_string()),
            library_uuid: Some(ALICE_UUID.to_string()),
        };
        receive(&db, offer).await;

        let copies = borrowed_copies(&db).await;
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].lender_peer_id, Some(alice));
        assert_ne!(
            copies[0].book_id, unrelated,
            "an empty ISBN must not attach the loan to an unrelated book"
        );

        let lent = crate::models::book::Entity::find_by_id(&copies[0].book_id)
            .one(&db)
            .await
            .expect("find")
            .expect("the loan created its own book");
        assert_eq!(lent.title, "Le Livre");
        assert_eq!(
            lent.isbn, None,
            "an empty ISBN is stored as NULL, so it never becomes a future collision"
        );
    }

    /// The other borrowed-copy writer guarded its ISBN *lookup* against the empty string
    /// but not its *write*, so it kept minting the empty-ISBN books that the lookups
    /// elsewhere collide with. Normalize once, at the edge.
    #[tokio::test(flavor = "multi_thread")]
    async fn borrower_acceptance_stores_an_empty_isbn_as_null() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        insert_legacy_request(&db, "out-1", alice, "").await;

        let payload = serde_json::json!({
            "title": "Le Livre",
            "isbn": "",
            "lender_name": "alice",
            "due_date": "2026-09-01",
        });
        process_borrower_acceptance(&db, "out-1", &payload, None).await;

        let copies = borrowed_copies(&db).await;
        assert_eq!(copies.len(), 1, "the borrowed copy is created");
        let lent = crate::models::book::Entity::find_by_id(&copies[0].book_id)
            .one(&db)
            .await
            .expect("find")
            .expect("book");
        assert_eq!(
            lent.isbn, None,
            "an empty ISBN is stored as NULL, never as an empty string"
        );
    }

    /// The legacy branch of `update_outgoing_status`: an outgoing request written before
    /// the `book_id` column falls back to the ISBN. A bare `.one()` on that ISBN names an
    /// arbitrary book — the borrower's own copy of a shared ISBN, or any row at all when
    /// the loan carries no ISBN (`receive_loan_offer` stores `""`). Purging is scoped to
    /// the lender, so the damage is bounded to a copy that lender really lent, but on the
    /// wrong book.
    ///
    /// Refusing to guess leaves a stale copy, which the user can remove. Guessing deletes
    /// a live loan of a different book, which they cannot undo.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_reclaim_refuses_to_guess_between_two_books_sharing_an_isbn() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;

        // Two book rows share the ISBN; Alice lent us a copy of each.
        let lent_first = insert_book_with_isbn(&db, "Le Livre", Some("978-same")).await;
        let lent_second = insert_book_with_isbn(&db, "Un Autre Livre", Some("978-same")).await;
        let copy_first = insert_peer_copy(&db, &lent_first, Some(alice)).await;
        let copy_second = insert_peer_copy(&db, &lent_second, Some(alice)).await;

        // A request predating the `book_id` column: only the ISBN identifies the loan.
        insert_legacy_request(&db, "legacy-req", alice, "978-same").await;
        mark_returned(&db, "legacy-req", ALICE_UUID).await;

        assert!(
            copy::Entity::find_by_id(copy_first.clone())
                .one(&db)
                .await
                .expect("find")
                .is_some()
                && copy::Entity::find_by_id(copy_second.clone())
                    .one(&db)
                    .await
                    .expect("find")
                    .is_some(),
            "an ambiguous ISBN must leave both loans untouched rather than guess"
        );
    }

    /// A loan of a book with no ISBN stores `book_isbn = ""`, and `create_borrowed_copy`
    /// can write that same empty string onto the book row. `Isbn.eq("")` then matches it,
    /// so an empty ISBN used as a search key names an unrelated book and purges its copy.
    /// (A book whose ISBN is NULL is safe by accident: SQL equality never matches NULL.)
    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_reclaim_with_an_empty_isbn_purges_nothing() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;

        let unrelated = insert_book_with_isbn(&db, "Un Autre Livre", Some("")).await;
        let unrelated_copy = insert_peer_copy(&db, &unrelated, Some(alice)).await;

        insert_legacy_request(&db, "legacy-req", alice, "").await;
        mark_returned(&db, "legacy-req", ALICE_UUID).await;

        assert!(
            copy::Entity::find_by_id(unrelated_copy)
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "an empty ISBN must not name an arbitrary book"
        );
    }

    /// The legacy path still works when the ISBN is unambiguous, including for a copy
    /// written before `lender_peer_id` existed. Guard against over-tightening the fix.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_legacy_reclaim_still_purges_an_unambiguous_loan() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;

        let book_id = insert_book_with_isbn(&db, "Le Livre", Some("978-unique")).await;
        let legacy_copy = insert_peer_copy(&db, &book_id, None).await; // pre-lender_peer_id

        insert_legacy_request(&db, "legacy-req", alice, "978-unique").await;
        mark_returned(&db, "legacy-req", ALICE_UUID).await;

        assert!(
            copy::Entity::find_by_id(legacy_copy)
                .one(&db)
                .await
                .expect("find")
                .is_none(),
            "an unambiguous legacy loan is still reclaimed"
        );
        assert!(
            crate::models::book::Entity::find_by_id(&book_id)
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "the book row survives, as on every return path"
        );
    }

    /// A reclaim arriving over the LAN must not delete the book row.
    ///
    /// `retain_returned_book` and `release_reclaimed_book` both refuse to: a book read
    /// without being owned is a first-class state carrying reading dates, a rating and
    /// notes the reader entered, and this runs on an inbound message with nobody in
    /// front of the screen. The plaintext twin used to delete it, so an unauthenticated
    /// LAN message destroyed data the user had typed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_reclaim_keeps_the_book_the_user_read() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;

        // A book the borrower read, rated and annotated, but never owned.
        let now = chrono::Utc::now().to_rfc3339();
        let book_id = crate::models::book::ActiveModel {
            title: Set("Le Livre".to_string()),
            isbn: Set(Some("978-same".to_string())),
            owned: Set(false),
            reading_status: Set("read".to_string()),
            user_rating: Set(Some(9)),
            finished_reading_at: Set(Some("2026-07-01".to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("insert book")
        .id;

        receive(&db, offer_from(ALICE_UUID, "alice-req")).await;

        let alice_request = p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::ToPeerId.eq(alice))
            .one(&db)
            .await
            .expect("query")
            .expect("alice's request");

        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path(alice_request.id.clone()),
            Json(serde_json::json!({ "status": "returned", "library_uuid": ALICE_UUID })),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);

        assert!(
            borrowed_copies(&db).await.is_empty(),
            "the borrowed copy is gone"
        );

        let book = crate::models::book::Entity::find_by_id(&book_id)
            .one(&db)
            .await
            .expect("find")
            .expect("the book the user read survives the reclaim");
        assert_eq!(book.user_rating, Some(9), "the rating survives");
        assert_eq!(
            book.finished_reading_at.as_deref(),
            Some("2026-07-01"),
            "the reading date survives"
        );
        assert!(!book.owned, "a reclaim never writes `owned`");
    }

    /// Idempotency survives the relaxation: the same lender's offer delivered twice
    /// (a replayed relay message, a retry) still yields a single copy.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_offer_from_the_same_peer_creates_no_second_copy() {
        let db = setup_db().await;
        insert_peer(&db, "alice", ALICE_UUID).await;

        receive(&db, offer_from(ALICE_UUID, "alice-req")).await;
        receive(&db, offer_from(ALICE_UUID, "alice-req")).await;

        assert_eq!(borrowed_copies(&db).await.len(), 1, "a replay adds no copy");
        assert_eq!(
            p2p_outgoing_request::Entity::find()
                .count(&db)
                .await
                .expect("count"),
            1,
            "a replay adds no outgoing request"
        );
    }

    /// An offer whose sender carries no resolvable identity leaves `lender_peer_id`
    /// NULL. A replay of it cannot be told apart from a new loan, so the old
    /// "at most one" rule is kept for NULL lenders rather than guessed at.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_unidentified_offer_creates_no_second_copy() {
        let db = setup_db().await;

        let anonymous = || LoanOffer {
            library_uuid: None,
            ..offer_from(ALICE_UUID, "alice-req")
        };
        receive(&db, anonymous()).await;
        receive(&db, anonymous()).await;

        let copies = borrowed_copies(&db).await;
        assert_eq!(copies.len(), 1, "an unidentified lender keeps one copy");
        assert_eq!(copies[0].lender_peer_id, None);
    }

    // ===== Sender identity on the plaintext P2P endpoints (ADR-050) =====
    //
    // The plaintext endpoints have no authenticated sender. Without the ownership check
    // any host on the LAN could name a request id and drive someone else's loan, purging
    // a borrowed copy, or delete a pending request. These tests pin the encrypted path's
    // invariant (`handle_status_update`, api/e2ee.rs) onto the plaintext twins.

    /// A peer that completed the key exchange, so it would only ever be trusted over the
    /// encrypted channel.
    async fn insert_peer_with_keys(db: &DatabaseConnection, name: &str, library_uuid: &str) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set(name.to_string()),
            url: Set(format!("http://{name}.local:8000")),
            library_uuid: Set(Some(library_uuid.to_string())),
            connection_status: Set("accepted".to_string()),
            key_exchange_done: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer")
        .id
    }

    async fn outgoing_status(db: &DatabaseConnection, id: &str) -> String {
        p2p_outgoing_request::Entity::find_by_id(id)
            .one(db)
            .await
            .expect("find")
            .expect("request")
            .status
    }

    async fn insert_incoming_request(db: &DatabaseConnection, id: &str, from_peer_id: i32) {
        let now = chrono::Utc::now().to_rfc3339();
        crate::models::p2p_request::ActiveModel {
            id: Set(id.to_string()),
            from_peer_id: Set(from_peer_id),
            book_isbn: Set("978-x".to_string()),
            book_title: Set("Le Livre".to_string()),
            status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            requester_request_id: Set(None),
        }
        .insert(db)
        .await
        .expect("insert incoming request");
    }

    async fn incoming_exists(db: &DatabaseConnection, id: &str) -> bool {
        crate::models::p2p_request::Entity::find_by_id(id)
            .one(db)
            .await
            .expect("find")
            .is_some()
    }

    /// No identity in the payload is anonymous: refuse rather than purge on a guessed id.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_status_update_without_identity_is_refused() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let book = insert_book_with_isbn(&db, "Le Livre", Some("978-x")).await;
        let copy_id = insert_peer_copy(&db, &book, Some(alice)).await;
        insert_legacy_request(&db, "req-1", alice, "978-x").await;

        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path("req-1".to_string()),
            Json(serde_json::json!({ "status": "returned" })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            copy::Entity::find_by_id(copy_id)
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "an anonymous status update must not purge the borrowed copy"
        );
        assert_eq!(outgoing_status(&db, "req-1").await, "accepted");
    }

    /// A paired peer naming another peer's loan is refused: the payload uuid must resolve
    /// to the very lender the request names.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_status_update_from_an_intruder_is_refused() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let _bob = insert_peer(&db, "bob", BOB_UUID).await;
        let book = insert_book_with_isbn(&db, "Le Livre", Some("978-x")).await;
        let copy_id = insert_peer_copy(&db, &book, Some(alice)).await;
        insert_legacy_request(&db, "req-1", alice, "978-x").await;

        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path("req-1".to_string()),
            Json(serde_json::json!({ "status": "returned", "library_uuid": BOB_UUID })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            copy::Entity::find_by_id(copy_id)
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "an intruder must not purge a copy borrowed from another peer"
        );
        assert_eq!(outgoing_status(&db, "req-1").await, "accepted");
    }

    /// A lender that completed the key exchange would use the encrypted channel, so a
    /// plaintext update naming its loan is refused even when it carries that lender's uuid.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_status_update_naming_a_key_exchanged_lender_is_refused() {
        let db = setup_db().await;
        let alice = insert_peer_with_keys(&db, "alice", ALICE_UUID).await;
        let book = insert_book_with_isbn(&db, "Le Livre", Some("978-x")).await;
        let copy_id = insert_peer_copy(&db, &book, Some(alice)).await;
        insert_legacy_request(&db, "req-1", alice, "978-x").await;

        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path("req-1".to_string()),
            Json(serde_json::json!({ "status": "returned", "library_uuid": ALICE_UUID })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            copy::Entity::find_by_id(copy_id)
                .one(&db)
                .await
                .expect("find")
                .is_some(),
            "a key-exchanged lender is served over E2EE; the plaintext twin must refuse"
        );
        assert_eq!(outgoing_status(&db, "req-1").await, "accepted");
    }

    /// Guard against over-tightening: the keyless lender the request names still closes it.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_lender_named_by_the_request_still_closes_the_loan() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let book = insert_book_with_isbn(&db, "Le Livre", Some("978-x")).await;
        let copy_id = insert_peer_copy(&db, &book, Some(alice)).await;
        insert_legacy_request(&db, "req-1", alice, "978-x").await;

        let response = update_outgoing_status(
            State(db.clone()),
            axum::extract::Path("req-1".to_string()),
            Json(serde_json::json!({ "status": "returned", "library_uuid": ALICE_UUID })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            copy::Entity::find_by_id(copy_id)
                .one(&db)
                .await
                .expect("find")
                .is_none(),
            "the real lender still reclaims its copy"
        );
        assert_eq!(outgoing_status(&db, "req-1").await, "returned");
    }

    /// A cancel with no identity is anonymous: it must not delete the request.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_cancel_without_identity_is_refused() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        insert_incoming_request(&db, "inc-1", alice).await;

        let response = cancel_request(
            State(db.clone()),
            axum::extract::Path("inc-1".to_string()),
            None,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            incoming_exists(&db, "inc-1").await,
            "an anonymous cancel must not delete the request"
        );
    }

    /// A cancel from a peer other than the request's author is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_plaintext_cancel_from_an_intruder_is_refused() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        let _bob = insert_peer(&db, "bob", BOB_UUID).await;
        insert_incoming_request(&db, "inc-1", alice).await;

        let response = cancel_request(
            State(db.clone()),
            axum::extract::Path("inc-1".to_string()),
            Some(Json(serde_json::json!({ "library_uuid": BOB_UUID }))),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            incoming_exists(&db, "inc-1").await,
            "an intruder must not cancel another peer's request"
        );
    }

    /// The borrower that authored the request still cancels its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_borrower_cancels_its_own_request() {
        let db = setup_db().await;
        let alice = insert_peer(&db, "alice", ALICE_UUID).await;
        insert_incoming_request(&db, "inc-1", alice).await;

        let response = cancel_request(
            State(db.clone()),
            axum::extract::Path("inc-1".to_string()),
            Some(Json(serde_json::json!({ "library_uuid": ALICE_UUID }))),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !incoming_exists(&db, "inc-1").await,
            "the author's own cancel still deletes the request"
        );
    }

    /// Decision 2: a first-contact library from an unauthenticated POST is created pending,
    /// never auto-trusted, so the auto-approve-loans option cannot fire for a stranger.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_contact_peer_is_created_pending_not_accepted() {
        let db = setup_db().await;
        let state = crate::infrastructure::AppState::new(db.clone());

        // The response code is orthogonal here (a stranger asking for a book we do not
        // own is legitimately auto-rejected); the peer is created either way, and its
        // status is what governs whether auto-approve-loans can ever fire for it.
        let _ = receive_request(
            State(state),
            Json(IncomingRequest {
                from_peer_url: "http://stranger.local:8000".to_string(),
                from_peer_name: "stranger".to_string(),
                book_isbn: "978-x".to_string(),
                book_title: "Le Livre".to_string(),
                requester_request_id: None,
            }),
        )
        .await
        .into_response();

        let created = peer::Entity::find()
            .filter(peer::Column::Url.eq("http://stranger.local:8000"))
            .one(&db)
            .await
            .expect("find")
            .expect("peer created");
        assert_eq!(
            created.connection_status, "pending",
            "a first-contact peer is not auto-trusted"
        );
        // `auto_approve == false` is precisely what makes the auto-approve-loans gate
        // (`is_auto_approve_loans_enabled && connection_status == \"accepted\"`) fail for a
        // stranger: the option can no longer fire on a first, unauthenticated contact.
        assert!(
            !created.auto_approve,
            "a first-contact peer does not auto-approve loans"
        );
    }

    /// The aligned inbound-request handler also creates a pending peer, regardless of the
    /// connection-validation toggle (off by default here).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_inbound_loan_request_creates_a_pending_peer() {
        let db = setup_db().await;

        let response = receive_loan_request(
            State(db.clone()),
            Json(IncomingLoanRequest {
                from_name: "stranger".to_string(),
                from_url: "http://stranger2.local:8000".to_string(),
                library_uuid: Some("stranger-uuid".to_string()),
                book_isbn: "978-x".to_string(),
                book_title: "Le Livre".to_string(),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let created = peer::Entity::find()
            .filter(peer::Column::Url.eq("http://stranger2.local:8000"))
            .one(&db)
            .await
            .expect("find")
            .expect("peer created");
        assert_eq!(created.connection_status, "pending");
        assert!(!created.auto_approve);
    }
}
