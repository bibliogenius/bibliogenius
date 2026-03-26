//! Enrich operation_log payloads at sync transport time.
//!
//! The operation_log stores minimal payloads (policy: no sensitive data).
//! When sending operations to a linked device, we read the current entity
//! state from the DB and build a complete payload for the receiver.
//!
//! Key additions:
//! - `book_isbn` + `book_title` injected into copy, book_note, collection_book,
//!   book_author, and book_tag payloads so the receiving device can resolve the
//!   local book_id by ISBN or title lookup (IDs are device-local).
//! - `author_name` / `tag_name` injected into junction payloads so the receiver
//!   can resolve by natural key instead of device-local auto-increment IDs.

use sea_orm::*;
use serde_json::{Value, json};

use crate::models::{author, book, contact, copy, operation_log, tag};

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
        // Junction tables: enrich with natural keys for cross-device resolution
        ("book_author", "insert") => enrich_book_author(db, &stored).await,
        ("book_tag", "insert") => enrich_book_tag(db, &stored).await,
        ("collection_book", "insert") => enrich_collection_book(db, &stored).await,
        // For DELETE, tags, authors, collections, etc.: use stored payload
        _ => stored,
    }
}

/// Enrich a book operation with fields needed by the processor on the receiver.
async fn enrich_book(db: &DatabaseConnection, book_id: i32) -> Option<Value> {
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

    // Fetch related tag names so the receiver can recreate book_tags
    let tag_names: Vec<String> = model
        .find_related(tag::Entity)
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect();

    // Parse subjects JSON for faithful sync of shelf assignments
    let subjects: Option<Vec<String>> = model
        .subjects
        .as_ref()
        .and_then(|s| serde_json::from_str(s).ok());

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
        "tags": tag_names,
        "subjects": subjects,
    }))
}

/// Enrich a copy INSERT with book_isbn + book_title for FK resolution on the receiver.
async fn enrich_copy_insert(db: &DatabaseConnection, copy_id: i32) -> Option<Value> {
    let c = copy::Entity::find_by_id(copy_id).one(db).await.ok()??;

    // Fetch the associated book for cross-device resolution (ISBN + title fallback)
    let related_book = book::Entity::find_by_id(c.book_id)
        .one(db)
        .await
        .ok()
        .flatten();
    let isbn = related_book.as_ref().and_then(|b| b.isbn.clone());
    let title = related_book.map(|b| b.title);

    Some(json!({
        "book_id": c.book_id,
        "book_isbn": isbn,
        "book_title": title,
        "library_id": c.library_id,
        "status": c.status,
        "notes": c.notes,
        "is_temporary": c.is_temporary,
    }))
}

/// Enrich a copy UPDATE with current status + book_isbn + book_title.
async fn enrich_copy_update(db: &DatabaseConnection, copy_id: i32) -> Option<Value> {
    let c = copy::Entity::find_by_id(copy_id).one(db).await.ok()??;

    let related_book = book::Entity::find_by_id(c.book_id)
        .one(db)
        .await
        .ok()
        .flatten();
    let isbn = related_book.as_ref().and_then(|b| b.isbn.clone());
    let title = related_book.map(|b| b.title);

    Some(json!({
        "book_isbn": isbn,
        "book_title": title,
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

/// Enrich a book_note operation with book_isbn + book_title for FK resolution.
async fn enrich_book_note(db: &DatabaseConnection, note_id: i32) -> Option<Value> {
    use crate::modules::book_notes::models as bn;

    let note = bn::Entity::find_by_id(note_id).one(db).await.ok()??;

    let related_book = book::Entity::find_by_id(note.book_id)
        .one(db)
        .await
        .ok()
        .flatten();
    let isbn = related_book.as_ref().and_then(|b| b.isbn.clone());
    let title = related_book.map(|b| b.title);

    Some(json!({
        "book_id": note.book_id,
        "book_isbn": isbn,
        "book_title": title,
        "content": note.content,
        "page": note.page,
    }))
}

/// Enrich a book_author junction INSERT with natural keys for cross-device resolution.
async fn enrich_book_author(db: &DatabaseConnection, stored: &Option<Value>) -> Option<Value> {
    let stored = stored.as_ref()?;
    let book_id = stored["book_id"].as_i64()? as i32;
    let author_id = stored["author_id"].as_i64()? as i32;

    let b = book::Entity::find_by_id(book_id).one(db).await.ok()??;
    let a = author::Entity::find_by_id(author_id).one(db).await.ok()??;

    Some(json!({
        "book_id": book_id,
        "author_id": author_id,
        "book_isbn": b.isbn,
        "book_title": b.title,
        "author_name": a.name,
    }))
}

/// Enrich a book_tag junction INSERT with natural keys for cross-device resolution.
async fn enrich_book_tag(db: &DatabaseConnection, stored: &Option<Value>) -> Option<Value> {
    let stored = stored.as_ref()?;
    let book_id = stored["book_id"].as_i64()? as i32;
    let tag_id = stored["tag_id"].as_i64()? as i32;

    let b = book::Entity::find_by_id(book_id).one(db).await.ok()??;
    let t = tag::Entity::find_by_id(tag_id).one(db).await.ok()??;

    Some(json!({
        "book_id": book_id,
        "tag_id": tag_id,
        "book_isbn": b.isbn,
        "book_title": b.title,
        "tag_name": t.name,
    }))
}

/// Enrich a collection_book INSERT with book natural keys for cross-device resolution.
async fn enrich_collection_book(db: &DatabaseConnection, stored: &Option<Value>) -> Option<Value> {
    let stored = stored.as_ref()?;
    let book_id = stored["book_id"]
        .as_i64()
        .or(stored["entity_id"].as_i64())? as i32;
    let collection_id = stored
        .get("_str_id")
        .or(stored.get("collection_id"))
        .and_then(|v| v.as_str())?
        .to_string();

    let b = book::Entity::find_by_id(book_id).one(db).await.ok()??;

    Some(json!({
        "_str_id": collection_id,
        "collection_id": collection_id,
        "book_id": book_id,
        "book_isbn": b.isbn,
        "book_title": b.title,
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
