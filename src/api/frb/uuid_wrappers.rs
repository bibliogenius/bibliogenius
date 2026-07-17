// Uuid-keyed wrappers delegating to the id-keyed handlers.
// Included by api/frb.rs (include!, not a module): items must stay in
// crate::api::frb so the generated bindings keep their names, and file order
// mirrors the include! order because the generated Dart facade follows
// declaration order. Shared imports live in frb.rs.

// ============ uuid-addressable entity API (transitional) ============
//
// The integer `id` is device-local (autoincrement), so it cannot identify a row
// across devices; the `uuid` column can. These thin wrappers let Flutter address
// the replicated entities by uuid: each resolves the uuid to the device-local id
// via the infrastructure lookups, then delegates to the existing id-based logic.
// They are additive: the id-based functions stay, so nothing breaks here. Once
// `uuid` becomes the primary key (the id column is dropped), the id leg disappears
// and this whole section is removed in favour of natively uuid-keyed services.
//
// Reference-bearing creators (e.g. create_loan, which takes a copy reference) are
// intentionally not migrated here: their references stay integer until the column
// types flip together with the database, so they are handled at that later step.

/// Fetch a book by its uuid (single fetch: resolves and enriches at once).
pub async fn get_book_by_uuid(uuid: String) -> Result<FrbBook, String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::book_service::get_book_by_uuid(db, &uuid).await {
        Ok(book) => Ok(FrbBook::from(book)),
        Err(crate::services::book_service::ServiceError::NotFound) => {
            Err("Book not found".to_string())
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Update a book identified by its uuid.
pub async fn update_book_by_uuid(uuid: String, book: FrbBook) -> Result<FrbBook, String> {
    let db = db().ok_or("Database not initialized")?;
    let book_dto: crate::models::Book = book.into();
    match crate::services::book_service::update_book(db, &uuid, book_dto).await {
        Ok(b) => Ok(FrbBook::from(b)),
        Err(crate::services::book_service::ServiceError::InvalidInput(m)) => Err(m),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Delete a book identified by its uuid.
pub async fn delete_book_by_uuid(uuid: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::book_service::delete_book(db, &uuid).await {
        Ok(_) => {
            // Notify peers that our catalog changed (same as create_book - HTTP handler bypassed).
            if let Some(state) = global_app_state() {
                crate::services::catalog_notification::schedule_catalog_changed_notification(
                    state.clone(),
                );
            }
            Ok(())
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Update a tag identified by its uuid.
pub async fn update_tag_by_uuid(
    uuid: String,
    name: String,
    parent_id: Option<String>,
) -> Result<FrbTag, String> {
    update_tag(uuid, name, parent_id).await
}

/// Delete a tag identified by its uuid.
pub async fn delete_tag_by_uuid(uuid: String) -> Result<(), String> {
    delete_tag(uuid).await
}

/// Fetch a contact by its uuid (single fetch, no id round-trip).
pub async fn get_contact_by_uuid(uuid: String) -> Result<FrbContact, String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::contact_service::get_contact_by_uuid(db, &uuid).await {
        Ok(contact) => Ok(FrbContact::from(contact)),
        Err(crate::services::contact_service::ServiceError::NotFound) => {
            Err("Contact not found".to_string())
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Delete a contact identified by its uuid.
pub async fn delete_contact_by_uuid(uuid: String) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::contact_service::delete_contact(db, &uuid).await {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{e:?}")),
    }
}

/// Mark the loan identified by its uuid as returned.
pub async fn return_loan_by_uuid(uuid: String) -> Result<String, String> {
    return_loan(uuid).await
}

/// Effective loan duration (days) for the book identified by its uuid.
pub async fn get_effective_loan_duration_by_book_uuid(book_uuid: String) -> Result<i32, String> {
    get_effective_loan_duration(book_uuid).await
}

/// Per-book loan duration override for the book identified by its uuid.
pub async fn get_book_loan_duration_by_book_uuid(book_uuid: String) -> Result<Option<i32>, String> {
    get_book_loan_duration(book_uuid).await
}

/// Set the per-book loan duration override for the book identified by its uuid.
pub async fn set_book_loan_duration_by_book_uuid(
    book_uuid: String,
    days: Option<i32>,
) -> Result<(), String> {
    set_book_loan_duration(book_uuid, days).await
}

/// Add the book identified by its uuid to a collection.
pub async fn add_book_to_collection_by_book_uuid(
    collection_id: String,
    book_uuid: String,
) -> Result<(), String> {
    add_book_to_collection(collection_id, book_uuid).await
}

/// Collections containing the book identified by its uuid.
pub async fn get_book_collections_by_book_uuid(
    book_uuid: String,
) -> Result<Vec<FrbCollection>, String> {
    get_book_collections(book_uuid).await
}

/// Notes attached to the book identified by its uuid.
pub async fn get_book_notes_by_book_uuid(book_uuid: String) -> Result<Vec<FrbBookNote>, String> {
    get_book_notes(book_uuid).await
}

/// Create a note on the book identified by its uuid.
pub async fn create_book_note_by_book_uuid(
    book_uuid: String,
    content: String,
    page: Option<i32>,
) -> Result<FrbBookNote, String> {
    create_book_note(book_uuid, content, page).await
}

/// Undo a metadata-fill batch for the book identified by its uuid.
pub async fn metadata_fill_undo_book_by_uuid(
    batch_id: String,
    book_uuid: String,
) -> Result<u32, String> {
    metadata_fill_undo_book(batch_id, book_uuid).await
}
