// Collection CRUD, series marking, volume numbers, view stats.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ---------------------------------------------------------------------------
// Collections FFI
// ---------------------------------------------------------------------------

/// Collection data exposed to Flutter.
pub struct FrbCollection {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub total_books: i64,
    pub owned_books: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl From<crate::domain::collection_repository::Collection> for FrbCollection {
    fn from(c: crate::domain::collection_repository::Collection) -> Self {
        FrbCollection {
            id: c.id,
            name: c.name,
            description: c.description,
            source: c.source,
            total_books: c.total_books,
            owned_books: c.owned_books,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// A book entry within a collection, exposed to Flutter.
pub struct FrbCollectionBook {
    pub book_id: String,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub added_at: String,
    pub is_owned: bool,
    pub digital_formats: Option<Vec<String>>,
    /// Personal reading status of the book (`to_read`, `reading`, `read`,
    /// `wanting`, `abandoned`). Drives the "unread = dimmed" frise rendering.
    pub reading_status: Option<String>,
    /// Reading-order position within a series-typed collection. `None` for
    /// unnumbered members (rendered after the numbered ones).
    pub volume_number: Option<i32>,
}

impl From<crate::domain::collection_repository::CollectionBook> for FrbCollectionBook {
    fn from(cb: crate::domain::collection_repository::CollectionBook) -> Self {
        FrbCollectionBook {
            book_id: cb.book_id,
            title: cb.title,
            author: cb.author,
            cover_url: cb.cover_url,
            publisher: cb.publisher,
            publication_year: cb.publication_year,
            added_at: cb.added_at,
            is_owned: cb.is_owned,
            digital_formats: cb.digital_formats,
            reading_status: cb.reading_status,
            volume_number: cb.volume_number,
        }
    }
}

// Helper macro to reduce boilerplate when constructing the collection repo.
macro_rules! collection_repo {
    ($db:expr) => {{
        use crate::infrastructure::repositories::collection_repository::SeaOrmCollectionRepository;
        SeaOrmCollectionRepository::new($db.clone())
    }};
}

/// Returns all collections with their book counts.
pub async fn get_all_collections() -> Result<Vec<FrbCollection>, String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.find_all()
        .await
        .map(|cs| cs.into_iter().map(FrbCollection::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Returns a single collection by ID, or None if not found.
pub async fn get_collection(id: String) -> Result<Option<FrbCollection>, String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.find_by_id(&id)
        .await
        .map(|opt| opt.map(FrbCollection::from))
        .map_err(|e| format!("{e:?}"))
}

/// Creates a new collection. Returns the created collection.
pub async fn create_collection(
    name: String,
    description: Option<String>,
) -> Result<FrbCollection, String> {
    use crate::domain::collection_repository::{CollectionRepository, CreateCollectionInput};
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    let input = CreateCollectionInput {
        name,
        description,
        source: Some("manual".to_string()),
    };
    repo.create(input)
        .await
        .map(FrbCollection::from)
        .map_err(|e| format!("{e:?}"))
}

/// Deletes a collection by ID. Books are left orphaned (current behaviour).
pub async fn delete_collection(id: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::collection_service::delete_collection(db, &id, false)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

/// Preview data for the "delete collection with its books" flow.
///
/// * `total_books` - books currently in the collection
/// * `to_delete` - books that would be deleted
/// * `to_keep` - books kept (loaned, borrowed, multi-collection, shelved)
pub struct FrbCollectionDeletionPreview {
    pub total_books: i64,
    pub to_delete: i64,
    pub to_keep: i64,
}

impl From<crate::services::collection_service::DeletionPreview> for FrbCollectionDeletionPreview {
    fn from(p: crate::services::collection_service::DeletionPreview) -> Self {
        FrbCollectionDeletionPreview {
            total_books: p.total_books,
            to_delete: p.to_delete,
            to_keep: p.to_keep,
        }
    }
}

/// Returns how many books would be deleted / kept if the collection were
/// removed along with its books.
pub async fn get_collection_deletion_preview(
    id: String,
) -> Result<FrbCollectionDeletionPreview, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::collection_service::preview_deletion(db, &id)
        .await
        .map(FrbCollectionDeletionPreview::from)
        .map_err(|e| format!("{e:?}"))
}

/// Deletes a collection along with its eligible books (no loaned/borrowed
/// copy, not in another collection, on no shelf). Returns the IDs of books
/// that were actually removed.
pub async fn delete_collection_with_books(id: String) -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::collection_service::delete_collection(db, &id, true)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Returns all books belonging to a collection.
pub async fn get_collection_books(collection_id: String) -> Result<Vec<FrbCollectionBook>, String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.get_books(&collection_id)
        .await
        .map(|bs| bs.into_iter().map(FrbCollectionBook::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Adds a book to a collection (idempotent).
pub async fn add_book_to_collection(collection_id: String, book_id: String) -> Result<(), String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.add_book(&collection_id, &book_id)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Removes a book from a collection.
pub async fn remove_book_from_collection(
    collection_id: String,
    book_id: String,
) -> Result<(), String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.remove_book(&collection_id, &book_id)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Marks a collection as a series (`source = 'series'`) or reverts it to a plain
/// manual collection. A series collection drives the reading-order frise on the
/// book-detail screen; membership and volume numbers are set with
/// `add_book_to_collection` + `set_book_volume_number`.
pub async fn mark_collection_as_series(
    collection_id: String,
    is_series: bool,
) -> Result<(), String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    let source = if is_series { "series" } else { "manual" };
    repo.set_source(&collection_id, source)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Sets (or clears, with `None`) a book's reading-order position within a
/// collection. No-op if the book is not a member. Used by the collection-detail
/// screen when numbering the volumes of a series.
pub async fn set_book_volume_number(
    collection_id: String,
    book_id: String,
    volume_number: Option<i32>,
) -> Result<(), String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.set_book_volume(&collection_id, &book_id, volume_number)
        .await
        .map_err(|e| format!("{e:?}"))
}

// ============ View Stats (FFI) ============

/// Get library view statistics (peer and follower views).
/// Returns a JSON string with total_peer, total_follower, total, and daily breakdown.
pub async fn get_library_view_stats() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::api::view_counter::get_view_stats(db).await
}

// ============ Collections (FFI) ============

/// Returns all collections a book belongs to.
pub async fn get_book_collections(book_id: String) -> Result<Vec<FrbCollection>, String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.get_book_collections(&book_id)
        .await
        .map(|cs| cs.into_iter().map(FrbCollection::from).collect())
        .map_err(|e| format!("{e:?}"))
}

/// Replaces the set of collections a book belongs to.
pub async fn update_book_collections(
    book_id: String,
    collection_ids: Vec<String>,
) -> Result<(), String> {
    use crate::domain::collection_repository::CollectionRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = collection_repo!(db);
    repo.update_book_collections(&book_id, collection_ids)
        .await
        .map_err(|e| format!("{e:?}"))
}
