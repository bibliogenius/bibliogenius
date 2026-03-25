//! Enrich operation_log payloads at sync transport time.
//!
//! The operation_log stores minimal payloads (policy: no sensitive data).
//! When sending operations to a linked device, we read the current entity
//! state from the DB and build a complete payload for the receiver.
//!
//! Key addition: `book_isbn` is injected into copy and book_note payloads
//! so the receiving device can resolve the local book_id by ISBN lookup
//! (IDs are device-local and differ between linked devices).

use sea_orm::*;
use serde_json::{Value, json};

use crate::models::{book, contact, copy, operation_log};

/// Build an enriched JSON payload for a sync operation by reading
/// the current entity state from the database.
///
/// Returns `None` if the entity no longer exists (e.g. deleted).
/// For operation types not requiring enrichment, returns the stored payload as-is.
pub async fn enrich_op_payload(
    db: &DatabaseConnection,
    op: &operation_log::Model,
) -> Option<Value> {
    let stored = op
        .payload
        .as_ref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());

    match (
        op.entity_type.to_lowercase().as_str(),
        op.operation.to_lowercase().as_str(),
    ) {
        ("book", "insert") | ("book", "create") => enrich_book(db, op.entity_id).await,
        ("book", "update") => enrich_book(db, op.entity_id).await,
        ("copy", "insert") => enrich_copy_insert(db, op.entity_id).await,
        ("copy", "update") => enrich_copy_update(db, op.entity_id).await,
        ("contact", "insert") => enrich_contact(db, op.entity_id).await,
        ("contact", "update") => enrich_contact(db, op.entity_id).await,
        ("book_note", "insert") | ("book_note", "update") => {
            enrich_book_note(db, op.entity_id).await
        }
        // For DELETE, junction tables, tags, authors, etc.: use stored payload
        _ => stored,
    }
}

/// Enrich a book operation with fields needed by the processor on the receiver.
async fn enrich_book(db: &DatabaseConnection, book_id: i32) -> Option<Value> {
    use crate::models::author;
    use sea_orm::ModelTrait;

    let model = book::Entity::find_by_id(book_id).one(db).await.ok()??;

    // Fetch related author names so the receiver can recreate the junction
    let author_names: Vec<String> = model
        .find_related(author::Entity)
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.name)
        .collect();

    Some(json!({
        "title": model.title,
        "isbn": model.isbn,
        "cover_url": model.cover_url,
        "owned": model.owned,
        "reading_status": model.reading_status,
        "summary": model.summary,
        "publisher": model.publisher,
        "publication_year": model.publication_year,
        "page_count": model.page_count,
        "authors": author_names,
    }))
}

/// Enrich a copy INSERT with book_isbn for FK resolution on the receiver.
async fn enrich_copy_insert(db: &DatabaseConnection, copy_id: i32) -> Option<Value> {
    let c = copy::Entity::find_by_id(copy_id).one(db).await.ok()??;

    // Fetch the associated book to get its ISBN (natural key for cross-device resolution)
    let isbn = book::Entity::find_by_id(c.book_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .and_then(|b| b.isbn);

    Some(json!({
        "book_id": c.book_id,
        "book_isbn": isbn,
        "library_id": c.library_id,
        "status": c.status,
        "notes": c.notes,
        "is_temporary": c.is_temporary,
    }))
}

/// Enrich a copy UPDATE with current status + book_isbn.
async fn enrich_copy_update(db: &DatabaseConnection, copy_id: i32) -> Option<Value> {
    let c = copy::Entity::find_by_id(copy_id).one(db).await.ok()??;

    let isbn = book::Entity::find_by_id(c.book_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .and_then(|b| b.isbn);

    Some(json!({
        "book_isbn": isbn,
        "status": c.status,
        "notes": c.notes,
    }))
}

/// Enrich a contact operation (no sensitive free text per policy).
async fn enrich_contact(db: &DatabaseConnection, contact_id: i32) -> Option<Value> {
    let c = contact::Entity::find_by_id(contact_id)
        .one(db)
        .await
        .ok()??;
    Some(json!({
        "type": c.r#type,
        "name": c.name,
        "first_name": c.first_name,
    }))
}

/// Enrich a book_note operation with book_isbn for FK resolution.
async fn enrich_book_note(db: &DatabaseConnection, note_id: i32) -> Option<Value> {
    use crate::modules::book_notes::models as bn;

    let note = bn::Entity::find_by_id(note_id).one(db).await.ok()??;

    let isbn = book::Entity::find_by_id(note.book_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .and_then(|b| b.isbn);

    Some(json!({
        "book_id": note.book_id,
        "book_isbn": isbn,
        "content": note.content,
        "page": note.page,
    }))
}

/// Build the enriched JSON representation of an operation for sync transport.
/// This is the single function called by device.rs and e2ee.rs when building
/// the ops array to send to a linked device.
pub async fn op_to_sync_json(db: &DatabaseConnection, op: &operation_log::Model) -> Value {
    let enriched_payload = enrich_op_payload(db, op).await;

    // Fall back to stored payload if enrichment returned None
    // (entity may have been deleted since the op was logged)
    let payload = enriched_payload.or_else(|| {
        op.payload
            .as_ref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
    });

    json!({
        "entity_type": op.entity_type,
        "entity_id": op.entity_id,
        "operation": op.operation,
        "payload": payload,
        "created_at": op.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;
    use sea_orm::{ActiveModelTrait, Set};

    async fn setup_db() -> DatabaseConnection {
        init_db("sqlite::memory:").await.expect("init_db failed")
    }

    #[tokio::test]
    async fn test_enrich_book_returns_payload() {
        let db = setup_db().await;

        let b = book::ActiveModel {
            title: Set("Le Petit Prince".to_string()),
            isbn: Set(Some("978-2070612758".to_string())),
            owned: Set(true),
            reading_status: Set("read".to_string()),
            cover_url: Set(None),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let book = b.insert(&db).await.unwrap();

        let payload = enrich_book(&db, book.id).await.unwrap();
        assert_eq!(payload["title"], "Le Petit Prince");
        assert_eq!(payload["isbn"], "978-2070612758");
        assert_eq!(payload["owned"], true);
    }

    #[tokio::test]
    async fn test_enrich_book_missing_returns_none() {
        let db = setup_db().await;
        assert!(enrich_book(&db, 9999).await.is_none());
    }

    #[tokio::test]
    async fn test_enrich_copy_includes_book_isbn() {
        let db = setup_db().await;

        // Create book
        let b = book::ActiveModel {
            title: Set("Test Book".to_string()),
            isbn: Set(Some("ISBN-COPY-TEST".to_string())),
            owned: Set(true),
            reading_status: Set("to_read".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let book = b.insert(&db).await.unwrap();

        // Create user + library (required FKs)
        let now = chrono::Utc::now().to_rfc3339();
        let user = crate::models::user::ActiveModel {
            username: Set("testuser".to_string()),
            password_hash: Set("hash".to_string()),
            role: Set("admin".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        let user = user.insert(&db).await.unwrap();
        let lib = crate::models::library::ActiveModel {
            name: Set("Test Lib".to_string()),
            owner_id: Set(user.id),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let library = lib.insert(&db).await.unwrap();

        // Create copy
        let c = copy::ActiveModel {
            book_id: Set(book.id),
            library_id: Set(library.id),
            status: Set("available".to_string()),
            is_temporary: Set(false),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let saved_copy = c.insert(&db).await.unwrap();

        let payload = enrich_copy_insert(&db, saved_copy.id).await.unwrap();
        assert_eq!(payload["book_isbn"], "ISBN-COPY-TEST");
        assert_eq!(payload["status"], "available");
        assert_eq!(payload["is_temporary"], false);
    }

    #[tokio::test]
    async fn test_enrich_book_note_includes_isbn() {
        use crate::modules::book_notes::models as bn;

        let db = setup_db().await;

        let b = book::ActiveModel {
            title: Set("Note Book".to_string()),
            isbn: Set(Some("ISBN-NOTE-TEST".to_string())),
            owned: Set(true),
            reading_status: Set("reading".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let book = b.insert(&db).await.unwrap();

        let n = bn::ActiveModel {
            book_id: Set(book.id),
            content: Set("A great passage on page 42".to_string()),
            page: Set(Some(42)),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let note = n.insert(&db).await.unwrap();

        let payload = enrich_book_note(&db, note.id).await.unwrap();
        assert_eq!(payload["book_isbn"], "ISBN-NOTE-TEST");
        assert_eq!(payload["content"], "A great passage on page 42");
        assert_eq!(payload["page"], 42);
    }
}
