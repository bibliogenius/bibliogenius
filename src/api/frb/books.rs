// Book CRUD: create, list, count.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ Books API ============

/// Create a new book
pub async fn create_book(book: FrbBook) -> Result<FrbBook, String> {
    println!("DEBUG FFI: create_book received: {:?}", book.title);
    if let Some(ref isbn) = book.isbn {
        println!("DEBUG FFI: create_book received ISBN: {}", isbn);
    } else {
        println!("DEBUG FFI: create_book received NO ISBN");
    }
    let db = db().ok_or("Database not initialized")?;
    let book_dto: crate::models::Book = book.into();

    match crate::services::book_service::create_book(db, book_dto).await {
        Ok(created_book) => {
            // Check achievements after book creation (e.g. first_book, collector badges)
            check_achievements(db).await;
            // Notify peers that our catalog changed. Fire-and-forget, debounced.
            // In FFI mode the HTTP handler in books.rs is bypassed, so we trigger
            // the notification here instead.
            if let Some(state) = global_app_state() {
                crate::services::catalog_notification::schedule_catalog_changed_notification(
                    state.clone(),
                );
            }
            Ok(FrbBook::from(created_book))
        }
        Err(crate::services::book_service::ServiceError::InvalidInput(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Get all books with optional filters
pub async fn get_all_books(
    status: Option<String>,
    title: Option<String>,
    tag: Option<String>,
) -> Result<Vec<FrbBook>, String> {
    let db = db().ok_or("Database not initialized")?;

    let filter = crate::services::book_service::BookFilter {
        status,
        title,
        tag,
        author: None,
    };

    match crate::services::book_service::list_books(db, filter).await {
        Ok(books) => Ok(books.into_iter().map(FrbBook::from).collect()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count total books
pub async fn count_books() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::book_service::count_books(db).await {
        Ok(count) => Ok(count),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Run the achievement check after a change that can unlock one.
///
/// Same block as the HTTP handlers run, kept here because the FFI path bypasses
/// them entirely. Best-effort by design: an achievement that fails to unlock
/// must never fail the write the reader asked for.
async fn check_achievements(db: &sea_orm::DatabaseConnection) {
    let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let hangman_repo =
        crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
    let _ = crate::services::gamification_service::check_and_unlock_achievements(
        &gamification_repo,
        &game_repo,
        Some(&puzzle_repo),
        Some(&hangman_repo),
    )
    .await;
}

// ============ Reading recorded on a book one does not own ============

/// What recording a reading changed, so the caller can say it in one sentence.
#[frb(dart_metadata=("freezed"))]
pub struct FrbReadRecord {
    pub book: FrbBook,
    /// The book was absent from the library and has just been created, not owned.
    pub created: bool,
    /// The book was already marked read: nothing was written.
    pub was_already_read: bool,
}

/// Record that the reader has read this book, whoever owns it.
///
/// Backs the "I have read it" button on someone else's catalogue: a book read
/// but never bought enters the library not owned and marked read, which is the
/// combination ADR-063 already filters and renders. Possession is never touched,
/// so a book the reader owns simply becomes read.
pub async fn record_read_book(book: FrbBook) -> Result<FrbReadRecord, String> {
    let db = db().ok_or("Database not initialized")?;
    let book_dto: crate::models::Book = book.into();

    let record = crate::services::book_service::record_read_book(db, book_dto)
        .await
        .map_err(|e| match e {
            crate::services::book_service::ServiceError::InvalidInput(msg) => msg,
            other => format!("{:?}", other),
        })?;

    if !record.was_already_read {
        // "Books read" is one of the counted dimensions, so a reading can unlock
        // a badge exactly like an acquisition does.
        check_achievements(db).await;
    }

    // A book the reader does not own reaches no peer: every outbound lane filters
    // on `owned`. The one payload change worth announcing is a book they DO own
    // leaving the wishlist, since `wanted` is the one reading state peers see.
    if !record.created
        && !record.was_already_read
        && record.book.owned == Some(true)
        && let Some(state) = global_app_state()
    {
        crate::services::catalog_notification::schedule_catalog_changed_notification(state.clone());
    }

    Ok(FrbReadRecord {
        created: record.created,
        was_already_read: record.was_already_read,
        book: FrbBook::from(record.book),
    })
}

/// What the reader's own library holds for one ISBN of someone else's shelf.
#[frb(dart_metadata=("freezed"))]
pub struct FrbLibraryIsbnStatus {
    /// Echoed in the form the caller asked about, so a client can index the
    /// list it is displaying with it.
    pub isbn: String,
    pub owned: bool,
    /// The stored value, verbatim: "read", "wanting", "to_read", "reading",
    /// "abandoned", or empty for no reading intent. The caller decides what to
    /// say about each.
    pub reading_status: String,
}

/// What my library holds for these ISBNs, for the ones it holds at all.
///
/// Reading someone else's shelves, the question is whether I already have this
/// book and whether I have already read it. Pass the ISBNs of the page on
/// display, never the whole catalogue: the cost is one query per call.
pub async fn get_library_isbn_status(
    isbns: Vec<String>,
) -> Result<Vec<FrbLibraryIsbnStatus>, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::book_service::library_status_for_isbns(db, &isbns)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|row| FrbLibraryIsbnStatus {
                    isbn: row.isbn,
                    owned: row.owned,
                    reading_status: row.reading_status,
                })
                .collect()
        })
        .map_err(|e| format!("{:?}", e))
}
