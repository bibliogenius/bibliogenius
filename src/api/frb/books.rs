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
            let _ = {
                let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
                let game_repo =
                    crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
                let puzzle_repo =
                    crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(
                        db.clone(),
                    );
                let hangman_repo =
                    crate::modules::hangman::repository::SeaOrmHangmanRepository::new(db.clone());
                crate::services::gamification_service::check_and_unlock_achievements(
                    &gamification_repo,
                    &game_repo,
                    Some(&puzzle_repo),
                    Some(&hangman_repo),
                )
                .await
            };
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
