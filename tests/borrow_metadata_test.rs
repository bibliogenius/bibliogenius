//! ADR-034: Copy loan metadata migration and repository writers.
//!
//! Covers:
//! - The `backfill_borrow_metadata` migration step that hydrates the new
//!   typed columns from legacy free-text `notes` strings.
//! - The `CopyRepository::create` path used by Flutter for contact loans
//!   populates the new columns when the caller provides them.
//! - The `create_copy` HTTP handler layer rejects invalid `borrow_source`.

use rust_lib_app::db;
use rust_lib_app::domain::{BorrowSource, CopyRepository, CreateCopyInput};
use rust_lib_app::infrastructure::repositories::SeaOrmCopyRepository;
use rust_lib_app::models::{Book, book, library, user};
use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, EntityTrait, Set, Statement};

async fn setup_db() -> DatabaseConnection {
    db::init_db("sqlite::memory:").await.expect("init db")
}

async fn seed_user_library_book(db: &DatabaseConnection) -> (i32, String) {
    let now = chrono::Utc::now().to_rfc3339();

    let u = user::ActiveModel {
        username: Set("alice".to_string()),
        password_hash: Set("!".to_string()),
        role: Set("admin".to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    let lib = library::ActiveModel {
        name: Set("Alice's Library".to_string()),
        owner_id: Set(u.id),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    let b = book::ActiveModel {
        title: Set("Martin Eden".to_string()),
        owned: Set(false),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();

    (lib.id, b.id)
}

async fn insert_legacy_borrowed(
    db: &DatabaseConnection,
    book_id: String,
    library_id: i32,
    notes: &str,
) {
    let now = chrono::Utc::now().to_rfc3339();
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT INTO copies (book_id, library_id, status, is_temporary, notes, uuid, created_at, updated_at) \
         VALUES (?, ?, 'borrowed', 1, ?, ?, ?, ?)",
        [
            book_id.into(),
            library_id.into(),
            notes.to_string().into(),
            // Raw insert bypasses before_save; set a uuid so model reads don't hit NULL.
            rust_lib_app::utils::uuid_gen::new_uuid_v7().into(),
            now.clone().into(),
            now.into(),
        ],
    ))
    .await
    .unwrap();
}

async fn fetch_copy_cols(
    db: &DatabaseConnection,
) -> (Option<String>, Option<String>, Option<String>, Option<i32>) {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT lender_display_name, borrow_due_date, borrow_source, lender_peer_id \
             FROM copies ORDER BY rowid DESC LIMIT 1"
                .to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    (
        row.try_get("", "lender_display_name").ok(),
        row.try_get("", "borrow_due_date").ok(),
        row.try_get("", "borrow_source").ok(),
        row.try_get("", "lender_peer_id").ok(),
    )
}

// -------- Backfill --------
//
// `db::backfill_borrow_metadata` reads/updates by `uuid` (present since migration
// 078), so it works after the ADR-044 uuid-PK migration dropped the integer
// `copies.id` column.

#[tokio::test(flavor = "multi_thread")]
async fn backfill_hydrates_peer_format() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    insert_legacy_borrowed(
        &db,
        book_id,
        lib_id,
        "Emprunté de Alice jusqu'au 2026-12-01",
    )
    .await;

    let stats = db::backfill_borrow_metadata(&db).await.unwrap();
    assert_eq!(stats.hydrated, 1);
    assert_eq!(stats.unparsed, 0);

    let (name, due, source, _) = fetch_copy_cols(&db).await;
    assert_eq!(name.as_deref(), Some("Alice"));
    assert_eq!(due.as_deref(), Some("2026-12-01"));
    assert_eq!(source.as_deref(), Some("peer"));
}

#[tokio::test(flavor = "multi_thread")]
async fn backfill_hydrates_contact_format_english() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    insert_legacy_borrowed(&db, book_id, lib_id, "Borrowed from Bob").await;

    let stats = db::backfill_borrow_metadata(&db).await.unwrap();
    assert_eq!(stats.hydrated, 1);

    let (name, due, source, _) = fetch_copy_cols(&db).await;
    assert_eq!(name.as_deref(), Some("Bob"));
    assert!(
        due.is_none(),
        "contact loans carry no due_date in legacy notes"
    );
    assert_eq!(source.as_deref(), Some("contact"));
}

#[tokio::test(flavor = "multi_thread")]
async fn backfill_leaves_unparseable_untouched() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    insert_legacy_borrowed(&db, book_id, lib_id, "Free-form user note about this copy").await;

    let stats = db::backfill_borrow_metadata(&db).await.unwrap();
    assert_eq!(stats.hydrated, 0);
    assert_eq!(stats.unparsed, 1);

    let (name, _, source, _) = fetch_copy_cols(&db).await;
    assert!(name.is_none());
    assert!(source.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn backfill_is_idempotent() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    insert_legacy_borrowed(
        &db,
        book_id,
        lib_id,
        "Emprunté de Alice jusqu'au 2026-12-01",
    )
    .await;

    let first = db::backfill_borrow_metadata(&db).await.unwrap();
    let second = db::backfill_borrow_metadata(&db).await.unwrap();
    assert_eq!(first.hydrated, 1);
    assert_eq!(
        second.hydrated, 0,
        "second run must not touch hydrated rows"
    );
}

// -------- Repository writer (contact-loan path) --------

#[tokio::test(flavor = "multi_thread")]
async fn repository_create_stores_typed_loan_columns() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;

    let repo = SeaOrmCopyRepository::new(db.clone());
    let created = repo
        .create(CreateCopyInput {
            book_id,
            library_id: lib_id,
            status: "borrowed".to_string(),
            is_temporary: false,
            lender_display_name: Some("Diane".to_string()),
            borrow_source: Some(BorrowSource::Contact.as_str().to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(created.lender_display_name.as_deref(), Some("Diane"));
    assert_eq!(created.borrow_source.as_deref(), Some("contact"));
    assert!(created.lender_peer_id.is_none());
    assert!(created.borrow_due_date.is_none());

    // Round-trip from DB to be sure the columns were persisted, not just
    // reflected from the input in memory.
    let (name, _, source, _) = fetch_copy_cols(&db).await;
    assert_eq!(name.as_deref(), Some("Diane"));
    assert_eq!(source.as_deref(), Some("contact"));
}

// -------- find_borrowed criterion --------
//
// The borrowed list and the dashboard counter read `find_borrowed`, while the
// library view reads `book_service::list_books`. Both answer the same question,
// "is this copy borrowed", so both must answer it on `status` alone. Scoping
// `find_borrowed` on `is_temporary` hid every copy borrowed from a contact,
// which is stored with `is_temporary = false` (ADR-034).

async fn insert_copy(
    repo: &SeaOrmCopyRepository,
    book_id: &str,
    library_id: i32,
    status: &str,
    is_temporary: bool,
    borrow_source: Option<BorrowSource>,
) -> String {
    repo.create(CreateCopyInput {
        book_id: book_id.to_string(),
        library_id,
        status: status.to_string(),
        is_temporary,
        borrow_source: borrow_source.map(|s| s.as_str().to_string()),
        ..Default::default()
    })
    .await
    .unwrap()
    .id
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn find_borrowed_lists_a_copy_borrowed_from_a_contact() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    let repo = SeaOrmCopyRepository::new(db.clone());

    // The shape `book_details_screen` writes: permanent copy row, contact source.
    let copy_id = insert_copy(
        &repo,
        &book_id,
        lib_id,
        "borrowed",
        false,
        Some(BorrowSource::Contact),
    )
    .await;

    let borrowed = repo.find_borrowed().await.unwrap();
    assert_eq!(
        borrowed.total, 1,
        "a book borrowed from a contact belongs in the borrowed list"
    );
    assert_eq!(borrowed.copies[0].id.as_deref(), Some(copy_id.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn find_borrowed_lists_every_provenance() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    let repo = SeaOrmCopyRepository::new(db.clone());

    // The three shapes a borrowed copy can take in the wild: contact loan
    // (ADR-034), peer loan, and a legacy row written before `borrow_source`.
    insert_copy(
        &repo,
        &book_id,
        lib_id,
        "borrowed",
        false,
        Some(BorrowSource::Contact),
    )
    .await;
    insert_copy(
        &repo,
        &book_id,
        lib_id,
        "borrowed",
        true,
        Some(BorrowSource::Peer),
    )
    .await;
    insert_copy(&repo, &book_id, lib_id, "borrowed", true, None).await;

    let borrowed = repo.find_borrowed().await.unwrap();
    assert_eq!(
        borrowed.total, 3,
        "provenance does not decide whether a copy is borrowed, status does"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn find_borrowed_ignores_a_copy_that_is_not_borrowed() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    let repo = SeaOrmCopyRepository::new(db.clone());

    insert_copy(&repo, &book_id, lib_id, "available", false, None).await;
    // A copy still flagged temporary but back on the shelf is not a loan.
    insert_copy(&repo, &book_id, lib_id, "available", true, None).await;

    let borrowed = repo.find_borrowed().await.unwrap();
    assert_eq!(borrowed.total, 0, "only `status = 'borrowed'` copies count");
}

// -------- populate_authors overlay criterion --------
//
// `Book::populate_authors` overlays `borrowed`/`lent` onto the DTO's
// `reading_status`, and the MCP assistant reads that overlaid field to answer
// "what have I borrowed?" (mcp_tool_service, contract section 3.5). Like
// `find_borrowed` and `book_service::list_books`, the overlay must decide
// "borrowed" on `status` alone. A copy borrowed from a contact is stored with
// `is_temporary = false` (ADR-034), so scoping the overlay on `is_temporary`
// silently hid every contact loan from the assistant.

#[tokio::test(flavor = "multi_thread")]
async fn populate_authors_marks_a_contact_borrow_as_borrowed() {
    let db = setup_db().await;
    let (lib_id, book_id) = seed_user_library_book(&db).await;
    let repo = SeaOrmCopyRepository::new(db.clone());

    // The shape `book_details_screen` writes: permanent copy, contact source.
    insert_copy(
        &repo,
        &book_id,
        lib_id,
        "borrowed",
        false,
        Some(BorrowSource::Contact),
    )
    .await;

    let model = book::Entity::find_by_id(book_id)
        .one(&db)
        .await
        .unwrap()
        .expect("seeded book exists");
    let dtos = Book::populate_authors(&db, vec![model]).await;

    assert_eq!(
        dtos[0].reading_status.as_deref(),
        Some("borrowed"),
        "a copy borrowed from a contact must surface as borrowed to the MCP assistant"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn borrow_source_enum_roundtrip() {
    assert_eq!(BorrowSource::Peer.as_str(), "peer");
    assert_eq!(BorrowSource::Contact.as_str(), "contact");
    assert_eq!("peer".parse::<BorrowSource>(), Ok(BorrowSource::Peer));
    assert_eq!("contact".parse::<BorrowSource>(), Ok(BorrowSource::Contact));
    assert!("external".parse::<BorrowSource>().is_err());
    assert!("".parse::<BorrowSource>().is_err());
}
