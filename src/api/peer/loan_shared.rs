//! Shared loan machinery: loan acceptance and borrowed-copy creation.

use crate::models::peer;
use axum::http::StatusCode;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde_json::json;
use tracing::info;

/// Result of a successful loan acceptance on the lender side.
pub(crate) struct LoanAcceptResult {
    pub lender_name: String,
    pub due_date: String,
    pub book_isbn: Option<String>,
    pub book_title: String,
    pub book_cover_url: Option<String>,
}

/// Resolve the effective loan duration (in days) for a given book.
///
/// Reads from `loan_settings` (global default + per-book override).
/// Falls back to 21 days if the settings table is unreachable.
pub(crate) async fn resolve_loan_duration_days(db: &DatabaseConnection, book_id: &str) -> i64 {
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;
    match repo.get_effective_duration(book_id).await {
        Ok(days) => days as i64,
        Err(e) => {
            tracing::warn!("Failed to read loan settings, using 21-day default: {e}");
            21
        }
    }
}

/// Core acceptance logic shared by plaintext and E2EE auto-approve paths.
///
/// Finds book/copy, creates contact/loan, updates copy status and request status.
/// Does NOT send notifications to the borrower (caller handles that).
pub(crate) async fn perform_loan_acceptance(
    db: &DatabaseConnection,
    request_id: &str,
    book_isbn: &str,
    book_title: &str,
    peer: &peer::Model,
) -> Result<LoanAcceptResult, String> {
    use crate::models::{book, contact, copy, loan, p2p_request};

    // 1. Find Book by ISBN (fallback to title)
    let book = match book::Entity::find()
        .filter(book::Column::Isbn.eq(book_isbn))
        .one(db)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => match book::Entity::find()
            .filter(book::Column::Title.eq(book_title))
            .one(db)
            .await
        {
            Ok(Some(b)) => b,
            _ => {
                return Err(format!(
                    "Book not found (ISBN: '{book_isbn}', Title: '{book_title}')"
                ));
            }
        },
        Err(e) => return Err(format!("DB error finding book: {e}")),
    };

    // 2. Find available copy
    let copy = match copy::Entity::find()
        .filter(copy::Column::BookId.eq(book.id.clone()))
        .filter(copy::Column::Status.eq("available"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        _ => return Err("No available copies".to_string()),
    };

    // 3. Find or create contact for peer
    let contact = match contact::Entity::find()
        .filter(contact::Column::Name.eq(&peer.name))
        .filter(contact::Column::Type.eq("Library"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            let lib_id = crate::utils::library_helpers::resolve_library_id(db)
                .await
                .map_err(|e| format!("No library: {e}"))?;
            let new_contact = contact::ActiveModel {
                r#type: Set("Library".to_string()),
                name: Set(peer.name.clone()),
                library_owner_id: Set(lib_id),
                is_active: Set(true),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            new_contact
                .insert(db)
                .await
                .map_err(|e| format!("Failed to create contact: {e}"))?
        }
        Err(e) => return Err(format!("DB error finding contact: {e}")),
    };

    // 4. Create loan
    let lib_id = crate::utils::library_helpers::resolve_library_id(db)
        .await
        .map_err(|e| format!("No library: {e}"))?;
    let duration_days = resolve_loan_duration_days(db, &book.id).await;
    let due = Utc::now() + chrono::Duration::days(duration_days);
    let loan = loan::ActiveModel {
        copy_id: Set(copy.id.clone()),
        contact_id: Set(contact.id),
        library_id: Set(lib_id),
        loan_date: Set(Utc::now().to_rfc3339()),
        due_date: Set(due.to_rfc3339()),
        status: Set("active".to_string()),
        created_at: Set(Utc::now().to_rfc3339()),
        updated_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    loan::Entity::insert(loan)
        .exec(db)
        .await
        .map_err(|e| format!("Failed to create loan: {e}"))?;

    // 5. Update copy status
    info!("Auto-approve: Updating copy {} status to 'loaned'", copy.id);
    let mut active_copy: copy::ActiveModel = copy.into();
    active_copy.status = Set("loaned".to_string());
    active_copy
        .update(db)
        .await
        .map_err(|e| format!("Failed to update copy status: {e}"))?;

    // 6. Update request status to accepted
    if let Ok(Some(req)) = p2p_request::Entity::find_by_id(request_id).one(db).await {
        let mut active_req: p2p_request::ActiveModel = req.into();
        active_req.status = Set("accepted".to_string());
        active_req.updated_at = Set(Utc::now().to_rfc3339());
        let _ = active_req.update(db).await;
    }

    // 7. Get lender name
    let lender_name = crate::utils::library_helpers::resolve_lender_display_name(db).await;

    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    Ok(LoanAcceptResult {
        lender_name,
        due_date: due.format("%Y-%m-%d").to_string(),
        book_isbn: book.isbn,
        book_title: book.title,
        // Loan-accept result is forwarded to the borrower through an E2EE
        // envelope that may travel via hub relay, so use the relay-safe
        // variant: local paths without a hub prefix are stripped to None.
        book_cover_url: crate::models::Book::safe_cover_url_for_relay(
            book.cover_url.as_deref(),
            &book.id,
            Some(book.updated_at.as_str()),
            hub_prefix.as_deref(),
        ),
    })
}

/// Parameters for creating a borrowed copy on the borrower side.
pub(crate) struct BorrowedCopyParams<'a> {
    pub title: &'a str,
    pub isbn: Option<&'a str>,
    pub author: Option<&'a str>,
    pub cover_url: Option<&'a str>,
    pub lender_name: &'a str,
    pub due_date: &'a str,
    /// Optional FK to `peers.id` — populated when the borrower knows which
    /// local peer row corresponds to the lender. Stored on the new
    /// `copies.lender_peer_id` column (ADR-034).
    pub lender_peer_id: Option<i32>,
    /// The lender's stable library identifier (`peers.library_uuid`), stored on
    /// the copy so a SECOND synced device can resolve the lender on return
    /// (ADR-049). A loan offer knows this even when the peer is not paired
    /// locally, so callers pass it directly; when absent, `create_borrowed_copy`
    /// falls back to the peer row named by `lender_peer_id`.
    pub lender_library_uuid: Option<&'a str>,
    /// The loan's id at the lender (`p2p_outgoing_request.lender_request_id`),
    /// copied onto the copy so the return notification survives on a device that
    /// never held the outgoing request (ADR-049).
    pub lender_request_id: Option<&'a str>,
}

/// Resolve the local `peers` row that a plaintext payload claims to come from.
///
/// Plaintext endpoints have no authenticated sender, so the only identity on offer
/// is the `library_uuid` the payload asserts. This is a lookup and never a create:
/// an unpaired sender must not be able to materialize a `peers` row by posting an
/// unauthenticated request. Returns `None` when the field is absent (an older
/// sender) or names a library we never paired with; callers degrade explicitly.
pub(crate) async fn resolve_peer_by_library_uuid(
    db: &DatabaseConnection,
    library_uuid: Option<&str>,
) -> Option<peer::Model> {
    let uuid = library_uuid.filter(|u| !u.is_empty())?;
    peer::Entity::find()
        .filter(peer::Column::LibraryUuid.eq(uuid))
        .one(db)
        .await
        .ok()
        .flatten()
}

/// Find the temporary copy already borrowed from `lender_peer_id` on this book row.
///
/// A book row is a bibliographic record carrying many copies, so it can legitimately
/// hold one borrowed copy per lender. The lender is therefore part of the identity of
/// a loan, and this is the idempotency key: a message replayed by the same lender
/// finds its copy, a second lender does not and gets one of their own.
///
/// Scoped on `borrow_source = 'peer'` rather than on `is_temporary` alone, so a copy
/// borrowed from a contact never blocks a peer loan of the same book. This matches
/// how `release_reclaimed_book` (`api/e2ee.rs`) scopes its purge: the guard that
/// produces the invariant and the purge that consumes it now filter alike.
///
/// A NULL `lender_peer_id` (a legacy copy, or an offer whose sender carried no
/// resolvable identity) cannot be told apart from a replay of itself, so it keeps the
/// older "at most one per book row" rule rather than being guessed at.
pub(crate) async fn find_peer_borrowed_copy(
    db: &DatabaseConnection,
    book_id: &str,
    lender_peer_id: Option<i32>,
) -> Option<crate::models::copy::Model> {
    use crate::models::copy;

    let query = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("borrowed"))
        .filter(copy::Column::IsTemporary.eq(true))
        .filter(copy::Column::BorrowSource.eq(crate::domain::BorrowSource::Peer.as_str()));

    let query = match lender_peer_id {
        Some(id) => query.filter(copy::Column::LenderPeerId.eq(id)),
        None => query.filter(copy::Column::LenderPeerId.is_null()),
    };

    query.one(db).await.ok().flatten()
}

/// Result of creating a borrowed copy.
pub(crate) struct BorrowedCopyResult {
    pub book_id: String,
    pub copy_id: String,
    /// `true` if an identical borrowed copy already existed (idempotency).
    pub already_existed: bool,
}

/// Find or create a book and its borrowed temporary copy on the borrower side.
///
/// Shared by `receive_loan_confirmation`, `handle_loan_confirmation` (e2ee.rs),
/// and the `loan_offer` handlers. Callers handle outgoing-request updates and
/// notifications themselves.
pub(crate) async fn create_borrowed_copy(
    db: &DatabaseConnection,
    params: &BorrowedCopyParams<'_>,
) -> Result<BorrowedCopyResult, (StatusCode, serde_json::Value)> {
    use crate::models::{book, copy};

    // An empty ISBN is not an ISBN. Peers send `"isbn": ""` as readily as they omit the
    // field, and `Isbn.eq("")` matches every row that stores the empty string, so the
    // loan would land on an unrelated book. Normalizing here covers both uses below: the
    // lookup no longer collides, and a book created for this loan stores NULL rather than
    // an empty string that a later lookup would collide with.
    let isbn = params.isbn.filter(|s| !s.is_empty());

    // 1. Find or create book
    let existing_book = if let Some(isbn_val) = isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn_val))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(params.title))
            .one(db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => {
            tracing::info!("Borrowed copy: book already exists id={}", b.id);

            // Update cover_url if the incoming one is HTTP and the existing
            // one is missing or a local file path (legacy borrowed books).
            if let Some(new_cover) = params.cover_url {
                let needs_update = new_cover.starts_with("http")
                    && !b
                        .cover_url
                        .as_deref()
                        .is_some_and(|u| u.starts_with("http"));
                if needs_update {
                    let mut active: book::ActiveModel = b.clone().into();
                    active.cover_url = Set(Some(new_cover.to_string()));
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    if let Err(e) = active.update(db).await {
                        tracing::warn!("Failed to update cover_url for book id={}: {e}", b.id);
                    } else {
                        tracing::info!(
                            "Updated cover_url for borrowed book id={} to HTTP URL",
                            b.id
                        );
                    }
                }
            }

            b.id
        }
        None => {
            let now = Utc::now().to_rfc3339();
            let summary_text = params.author.map(|a| format!("Auteur: {a}"));
            let new_book = book::ActiveModel {
                title: Set(params.title.to_string()),
                isbn: Set(isbn.map(|s| s.to_string())),
                summary: Set(summary_text),
                cover_url: Set(params.cover_url.map(|s| s.to_string())),
                owned: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            match new_book.insert(db).await {
                Ok(b) => {
                    tracing::info!("Borrowed copy: created book id={}", b.id);
                    b.id
                }
                Err(e) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({ "error": format!("Failed to create book: {e}") }),
                    ));
                }
            }
        }
    };

    // ADR-049: the lender identity to stamp on the copy, resolved once for both
    // the create and the idempotent-replay paths below. Prefer the value the caller
    // supplies (a loan offer knows it even when the peer is not paired locally);
    // otherwise read it from the local peer row named by `lender_peer_id`.
    let lender_library_uuid = match params.lender_library_uuid.filter(|u| !u.is_empty()) {
        Some(u) => Some(u.to_string()),
        None => match params.lender_peer_id {
            Some(pid) => crate::models::peer::Entity::find_by_id(pid)
                .one(db)
                .await
                .ok()
                .flatten()
                .and_then(|p| p.library_uuid),
            None => None,
        },
    };
    let lender_request_id = params.lender_request_id.map(|s| s.to_string());

    // 2. Idempotency: skip if this lender already lent us a copy of this book.
    if let Some(existing) = find_peer_borrowed_copy(db, &book_id, params.lender_peer_id).await {
        tracing::info!(
            "Borrowed copy already exists (id={}) for book_id={} from lender {:?}, skipping",
            existing.id,
            book_id,
            params.lender_peer_id
        );
        // Backfill the ADR-049 identity onto a copy created before these columns
        // carried a value (a pre-089 borrow, or an earlier confirmation without the
        // ids). Fill only NULL fields, never overwrite, and best-effort so a replay
        // never fails the borrow. The lender is the same — `find_peer_borrowed_copy`
        // is scoped by `lender_peer_id` — so the values cannot conflict.
        let fill_uuid = existing.lender_library_uuid.is_none() && lender_library_uuid.is_some();
        let fill_req = existing.lender_request_id.is_none() && lender_request_id.is_some();
        if fill_uuid || fill_req {
            let mut active: copy::ActiveModel = existing.clone().into();
            if fill_uuid {
                active.lender_library_uuid = Set(lender_library_uuid.clone());
            }
            if fill_req {
                active.lender_request_id = Set(lender_request_id.clone());
            }
            active.updated_at = Set(Utc::now().to_rfc3339());
            if let Err(e) = active.update(db).await {
                tracing::warn!(
                    "Failed to backfill lender identity on existing copy {}: {e}",
                    existing.id
                );
            }
        }
        return Ok(BorrowedCopyResult {
            book_id,
            copy_id: existing.id,
            already_existed: true,
        });
    }

    // 3. Create borrowed temporary copy
    let lib_id = crate::utils::library_helpers::resolve_library_id(db)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("No library: {e}") }),
            )
        })?;

    let now = Utc::now().to_rfc3339();
    // ADR-034: lender metadata lives in typed columns. `notes` stays free for
    // real user notes; the migration-075 backfill hydrates these columns for
    // rows written before this change.
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id.clone()),
        library_id: Set(lib_id),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        lender_display_name: Set(Some(params.lender_name.to_string())),
        lender_peer_id: Set(params.lender_peer_id),
        // ADR-049: the stable lender identity and the loan's id at the lender,
        // both needed to notify the lender from any of the borrower's devices.
        lender_library_uuid: Set(lender_library_uuid),
        lender_request_id: Set(lender_request_id),
        borrow_due_date: Set(Some(params.due_date.to_string())),
        borrow_source: Set(Some(crate::domain::BorrowSource::Peer.as_str().to_string())),
        acquisition_date: Set(Some(now.clone())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(db).await {
        Ok(c) => {
            tracing::info!("Created borrowed copy id={} for book_id={}", c.id, book_id);
            Ok(BorrowedCopyResult {
                book_id,
                copy_id: c.id,
                already_existed: false,
            })
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("Failed to create copy: {e}") }),
        )),
    }
}

/// ADR-049: `create_borrowed_copy` records the lender's stable identity and the
/// loan id on the copy at borrow time, so a return from any of the borrower's
/// synced devices can notify the lender.
#[cfg(test)]
mod create_borrowed_copy_lender_identity_tests {
    use super::*;
    use crate::db;
    use crate::models::{copy, peer};
    use sea_orm::Set;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer_with_uuid(db: &DatabaseConnection, uuid: &str) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let p = peer::ActiveModel {
            name: Set("Bob".to_string()),
            url: Set("http://bob.local:8080".to_string()),
            library_uuid: Set(Some(uuid.to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        peer::Entity::insert(p)
            .exec(db)
            .await
            .unwrap()
            .last_insert_id
    }

    async fn fetch_copy(db: &DatabaseConnection, copy_id: &str) -> copy::Model {
        copy::Entity::find_by_id(copy_id.to_string())
            .one(db)
            .await
            .unwrap()
            .expect("copy exists")
    }

    // A peer loan resolves the lender's stable identity from the paired peer row
    // when the caller does not supply it, and records the loan id.
    #[tokio::test]
    async fn peer_loan_records_lender_uuid_from_the_peer_and_the_request_id() {
        let db = setup().await;
        let peer_id = insert_peer_with_uuid(&db, "lib-bob").await;

        let params = BorrowedCopyParams {
            title: "Dune",
            isbn: Some("978-1"),
            author: None,
            cover_url: None,
            lender_name: "Bob",
            due_date: "2026-08-01",
            lender_peer_id: Some(peer_id),
            lender_library_uuid: None, // not supplied: resolve from the peer row
            lender_request_id: Some("req-42"),
        };
        let result = create_borrowed_copy(&db, &params).await.expect("create");
        let copy = fetch_copy(&db, &result.copy_id).await;

        assert_eq!(copy.lender_library_uuid.as_deref(), Some("lib-bob"));
        assert_eq!(copy.lender_request_id.as_deref(), Some("req-42"));
    }

    // The core ADR-049 win: an offer from a peer NOT paired locally still stores
    // the library_uuid the payload carried, so a second synced device that DOES
    // know the peer can complete the return notification.
    #[tokio::test]
    async fn offer_from_an_unpaired_peer_still_stores_the_supplied_library_uuid() {
        let db = setup().await;

        let params = BorrowedCopyParams {
            title: "Neuromancer",
            isbn: Some("978-2"),
            author: None,
            cover_url: None,
            lender_name: "Carol",
            due_date: "2026-08-01",
            lender_peer_id: None, // peer unknown on this device
            lender_library_uuid: Some("lib-carol"),
            lender_request_id: Some("req-7"),
        };
        let result = create_borrowed_copy(&db, &params).await.expect("create");
        let copy = fetch_copy(&db, &result.copy_id).await;

        assert_eq!(copy.lender_peer_id, None);
        assert_eq!(copy.lender_library_uuid.as_deref(), Some("lib-carol"));
        assert_eq!(copy.lender_request_id.as_deref(), Some("req-7"));
    }

    // A second confirmation of the same loan (idempotent replay) backfills the
    // identity fields that were NULL on the first pass, without overwriting what is
    // already set. Covers a copy borrowed before a confirmation carried the ids.
    #[tokio::test]
    async fn idempotent_replay_backfills_only_the_null_lender_fields() {
        let db = setup().await;
        let peer_id = insert_peer_with_uuid(&db, "lib-bob").await;

        let base = BorrowedCopyParams {
            title: "Dune",
            isbn: Some("978-1"),
            author: None,
            cover_url: None,
            lender_name: "Bob",
            due_date: "2026-08-01",
            lender_peer_id: Some(peer_id),
            lender_library_uuid: None,
            lender_request_id: None, // first pass: no loan id yet
        };
        let first = create_borrowed_copy(&db, &base).await.expect("create");
        assert!(!first.already_existed);
        // uuid resolved from the peer; request id still absent.
        let copy = fetch_copy(&db, &first.copy_id).await;
        assert_eq!(copy.lender_library_uuid.as_deref(), Some("lib-bob"));
        assert_eq!(copy.lender_request_id, None);

        // Second pass for the same lender + book: the loan id now arrives.
        let replay = BorrowedCopyParams {
            lender_request_id: Some("req-42"),
            ..base
        };
        let second = create_borrowed_copy(&db, &replay).await.expect("replay");
        assert!(second.already_existed);
        assert_eq!(second.copy_id, first.copy_id, "same copy, no duplicate");

        let copy = fetch_copy(&db, &first.copy_id).await;
        assert_eq!(
            copy.lender_request_id.as_deref(),
            Some("req-42"),
            "backfilled"
        );
        assert_eq!(
            copy.lender_library_uuid.as_deref(),
            Some("lib-bob"),
            "existing value untouched"
        );
    }
}
