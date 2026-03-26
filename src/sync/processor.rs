use crate::models::{
    author, book, collection, collection_book, contact, copy, loan, operation_log, tag,
};
use sea_orm::*;
use serde_json::Value;
use std::time::Duration;

pub async fn run_processor(db: DatabaseConnection) {
    tracing::info!("🔄 Operation Processor started");

    loop {
        match process_next_batch(&db).await {
            Err(e) => {
                tracing::error!("❌ Error processing operations: {}", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            _ => {
                // If no operations were found, verify inside process_next_batch if we should sleep
                // For simplicity here, we assume process_next_batch handles the sleep if empty
            }
        }
    }
}

async fn process_next_batch(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Fetch one pending operation (FIFO, deterministic: created_at then id)
    let pending_op = operation_log::Entity::find()
        .filter(operation_log::Column::Status.eq("pending"))
        .order_by_asc(operation_log::Column::CreatedAt)
        .order_by_asc(operation_log::Column::Id)
        .one(db)
        .await?;

    match pending_op {
        Some(op) => {
            // Skip local operations — they are already applied by the handler that created them.
            // Only operations received from peers (source != "local") need to be replayed.
            if op.source == "local" {
                let mut active_op: operation_log::ActiveModel = op.into();
                active_op.status = Set("applied".to_string());
                active_op.save(db).await?;
                return Ok(());
            }

            tracing::info!(
                "⚙️ Processing Op #{}: {} on {} {}",
                op.id,
                op.operation,
                op.entity_type,
                op.entity_id
            );
            apply_operation(db, op).await?;
            // Don't sleep, process next immediately
        }
        None => {
            // No pending operations, sleep a bit
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    Ok(())
}

async fn apply_operation(db: &DatabaseConnection, op: operation_log::Model) -> Result<(), DbErr> {
    // Start transaction
    let txn = db.begin().await?;

    let result = match (
        op.entity_type.to_lowercase().as_str(),
        op.operation.to_lowercase().as_str(),
    ) {
        // Books
        ("book", "create") | ("book", "insert") => apply_book_create(&txn, &op).await,
        ("book", "update") => apply_book_update(&txn, &op).await,
        ("book", "delete") => apply_delete::<book::Entity>(&txn, op.entity_id).await,
        // Copies
        ("copy", "insert") => apply_copy_create(&txn, &op).await,
        ("copy", "update") => apply_copy_update(&txn, &op).await,
        ("copy", "delete") => apply_delete::<copy::Entity>(&txn, op.entity_id).await,
        // Contacts
        ("contact", "insert") => apply_contact_create(&txn, &op).await,
        ("contact", "update") => apply_contact_update(&txn, &op).await,
        ("contact", "delete") => apply_delete::<contact::Entity>(&txn, op.entity_id).await,
        // Loans
        ("loan", "insert") => apply_loan_create(&txn, &op).await,
        ("loan", "update") => apply_loan_update(&txn, &op).await,
        // Tags
        ("tag", "insert") => apply_tag_create(&txn, &op).await,
        ("tag", "delete") => apply_delete::<tag::Entity>(&txn, op.entity_id).await,
        // Authors
        ("author", "insert") => apply_author_create(&txn, &op).await,
        ("author", "delete") => apply_delete::<author::Entity>(&txn, op.entity_id).await,
        // Book-Author / Book-Tag junction tables (resolved by natural keys)
        ("book_author", "insert") => apply_book_author_insert(&txn, &op).await,
        ("book_author", "delete") => apply_book_author_delete(&txn, &op).await,
        ("book_tag", "insert") => apply_book_tag_insert(&txn, &op).await,
        ("book_tag", "delete") => apply_book_tag_delete(&txn, &op).await,
        // Collections (string UUID IDs)
        ("collection", "insert") => apply_collection_create(&txn, &op).await,
        ("collection", "delete") => apply_collection_delete(&txn, &op).await,
        ("collection_book", "insert") => apply_collection_book_insert(&txn, &op).await,
        ("collection_book", "delete") => apply_collection_book_delete(&txn, &op).await,
        // Book notes (device sync only)
        ("book_note", "insert") => apply_book_note_create(&txn, &op).await,
        ("book_note", "update") => apply_book_note_update(&txn, &op).await,
        ("book_note", "delete") => {
            apply_delete::<crate::modules::book_notes::models::Entity>(&txn, op.entity_id).await
        }
        _ => {
            tracing::warn!(
                "Unhandled operation type: {} {}",
                op.entity_type,
                op.operation
            );
            Ok(()) // Mark as applied to avoid stuck loop
        }
    };

    let mut active_op: operation_log::ActiveModel = op.into();

    match result {
        Ok(_) => {
            active_op.status = Set("applied".to_string());
            active_op.error_message = Set(None);
        }
        Err(e) => {
            tracing::error!("❌ Apply Failed: {}", e);
            active_op.status = Set("failed".to_string());
            active_op.error_message = Set(Some(e.to_string()));
        }
    }

    active_op.save(&txn).await?;
    txn.commit().await?;

    Ok(())
}

async fn apply_book_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload_str = op
        .payload
        .as_ref()
        .ok_or(DbErr::Custom("No payload".to_string()))?;
    let payload: Value =
        serde_json::from_str(payload_str).map_err(|e| DbErr::Custom(e.to_string()))?;

    let title = payload["title"].as_str().unwrap_or("Unknown").to_string();
    let isbn = payload["isbn"].as_str().map(|s| s.to_string());

    // Deduplication: skip if a book with the same ISBN already exists
    if let Some(ref isbn_val) = isbn
        && !isbn_val.is_empty()
    {
        let existing = book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn_val.clone()))
            .one(db)
            .await?;
        if existing.is_some() {
            tracing::info!("Skipping duplicate book (ISBN {isbn_val}): {title}");
            return Ok(());
        }
    }

    // Deduplication fallback: skip if exact title match exists (for books without ISBN)
    if isbn.is_none() || isbn.as_deref() == Some("") {
        let existing = book::Entity::find()
            .filter(book::Column::Title.eq(title.clone()))
            .one(db)
            .await?;
        if existing.is_some() {
            tracing::info!("Skipping duplicate book (title match): {title}");
            return Ok(());
        }
    }

    let owned = payload["owned"].as_bool().unwrap_or(true);
    let reading_status = payload["reading_status"]
        .as_str()
        .unwrap_or("to_read")
        .to_string();
    let cover_url = payload["cover_url"].as_str().map(|s| s.to_string());

    let summary = payload["summary"].as_str().map(|s| s.to_string());
    let publisher = payload["publisher"].as_str().map(|s| s.to_string());
    let publication_year = payload["publication_year"].as_i64().map(|v| v as i32);
    let page_count = payload["page_count"].as_i64().map(|v| v as i32);
    let now = chrono::Utc::now().to_rfc3339();

    // Restore subjects (shelf/tag assignments stored as JSON array in the book)
    let subjects = payload.get("subjects").and_then(|v| {
        if v.is_null() {
            None
        } else {
            Some(v.to_string())
        }
    });

    let new_book = book::ActiveModel {
        title: Set(title),
        isbn: Set(isbn),
        summary: Set(summary),
        publisher: Set(publisher),
        publication_year: Set(publication_year),
        page_count: Set(page_count),
        owned: Set(owned),
        reading_status: Set(reading_status.clone()),
        cover_url: Set(cover_url),
        subjects: Set(subjects),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };

    let insert_result = book::Entity::insert(new_book).exec(db).await?;
    let local_book_id = insert_result.last_insert_id;

    // Create author records and book_authors junction entries
    if let Some(author_names) = payload["authors"].as_array() {
        for name_val in author_names {
            if let Some(name) = name_val.as_str() {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                let author_id = find_or_create_author(db, name, &now).await?;
                let _ = db
                    .execute(Statement::from_sql_and_values(
                        db.get_database_backend(),
                        "INSERT OR IGNORE INTO book_authors (book_id, author_id) VALUES ($1, $2)",
                        [local_book_id.into(), author_id.into()],
                    ))
                    .await;
            }
        }
    }

    // Create tag records and book_tags junction entries
    if let Some(tag_names) = payload["tags"].as_array() {
        for name_val in tag_names {
            if let Some(name) = name_val.as_str() {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                let tag_id = find_or_create_tag(db, name, &now).await?;
                let _ = db
                    .execute(Statement::from_sql_and_values(
                        db.get_database_backend(),
                        "INSERT OR IGNORE INTO book_tags (book_id, tag_id) VALUES ($1, $2)",
                        [local_book_id.into(), tag_id.into()],
                    ))
                    .await;
            }
        }
    }

    // Create default copy for owned books (matches local book creation behavior)
    if owned {
        let lib_id = crate::utils::library_helpers::resolve_library_id(db).await?;
        // Dedup: skip if a copy already exists for this book
        let existing_copy = copy::Entity::find()
            .filter(copy::Column::BookId.eq(local_book_id))
            .one(db)
            .await?;
        if existing_copy.is_none() {
            let new_copy = copy::ActiveModel {
                book_id: Set(local_book_id),
                library_id: Set(lib_id),
                status: Set("available".to_string()),
                is_temporary: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            let _ = copy::Entity::insert(new_copy).exec(db).await;
        }
    }

    Ok(())
}

/// Find an author by name or create one. Returns the author's local ID.
async fn find_or_create_author(
    db: &DatabaseTransaction,
    name: &str,
    now: &str,
) -> Result<i32, DbErr> {
    if let Some(existing) = author::Entity::find()
        .filter(author::Column::Name.eq(name))
        .one(db)
        .await?
    {
        return Ok(existing.id);
    }
    let new_author = author::ActiveModel {
        name: Set(name.to_string()),
        created_at: Set(now.to_string()),
        updated_at: Set(now.to_string()),
        ..Default::default()
    };
    let res = author::Entity::insert(new_author).exec(db).await?;
    Ok(res.last_insert_id)
}

/// Find a tag by name or create one. Returns the tag's local ID.
async fn find_or_create_tag(db: &DatabaseTransaction, name: &str, now: &str) -> Result<i32, DbErr> {
    if let Some(existing) = tag::Entity::find()
        .filter(tag::Column::Name.eq(name))
        .one(db)
        .await?
    {
        return Ok(existing.id);
    }
    let new_tag = tag::ActiveModel {
        name: Set(name.to_string()),
        path: Set(name.to_string()),
        created_at: Set(now.to_string()),
        updated_at: Set(now.to_string()),
        ..Default::default()
    };
    let res = tag::Entity::insert(new_tag).exec(db).await?;
    Ok(res.last_insert_id)
}

/// Generic helper to parse payload JSON from an operation
fn parse_payload(op: &operation_log::Model) -> Result<Value, DbErr> {
    let payload_str = op
        .payload
        .as_ref()
        .ok_or(DbErr::Custom("No payload".to_string()))?;
    serde_json::from_str(payload_str).map_err(|e| DbErr::Custom(e.to_string()))
}

/// Generic delete by entity ID (works for any entity with i32 PK)
async fn apply_delete<E>(db: &DatabaseTransaction, id: i32) -> Result<(), DbErr>
where
    E: EntityTrait,
    E::PrimaryKey: PrimaryKeyTrait<ValueType = i32>,
{
    E::delete_by_id(id).exec(db).await?;
    Ok(())
}

async fn apply_book_update(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    // Retrieve existing book
    let book = book::Entity::find_by_id(op.entity_id).one(db).await?;

    if let Some(existing_book) = book {
        // CONFLICT RESOLUTION (Last-Write-Wins)
        // Compare op.created_at with existing_book.updated_at
        // For now, we assume Op is authoritative for demo purposes

        let payload_str = op
            .payload
            .as_ref()
            .ok_or(DbErr::Custom("No payload".to_string()))?;
        let payload: Value =
            serde_json::from_str(payload_str).map_err(|e| DbErr::Custom(e.to_string()))?;

        let mut active_book: book::ActiveModel = existing_book.into();

        if let Some(t) = payload.get("title").and_then(|v| v.as_str()) {
            active_book.title = Set(t.to_string());
        }

        active_book.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active_book.save(db).await?;
    } else {
        // Update for missing book? Treat as create or ignore?
        tracing::warn!("Update target not found: Book {}", op.entity_id);
    }

    Ok(())
}

// ── Shared helper: resolve book_id by ISBN lookup ────────────────────

/// Resolve a local book_id from a sync payload.
///
/// Priority:
/// 1. `book_isbn` field -> lookup local book by ISBN (cross-device safe)
/// 2. `book_title` field -> lookup local book by exact title (cross-device safe)
/// 3. `book_id` field -> raw ID (backward compat with backfill data on same device)
///
/// Returns `None` if the book cannot be found locally.
async fn resolve_local_book_id(
    db: &DatabaseTransaction,
    payload: &serde_json::Value,
) -> Result<Option<i32>, DbErr> {
    // Priority 1: ISBN-based lookup (works across devices)
    if let Some(isbn) = payload
        .get("book_isbn")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        let local_book = book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn))
            .one(db)
            .await?;
        if let Some(b) = local_book {
            return Ok(Some(b.id));
        }
    }

    // Priority 2: title-based lookup (for books without ISBN)
    if let Some(title) = payload
        .get("book_title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        let local_book = book::Entity::find()
            .filter(book::Column::Title.eq(title))
            .one(db)
            .await?;
        if let Some(b) = local_book {
            return Ok(Some(b.id));
        }
    }

    // Fallback: use raw book_id (works for same-device backfill data)
    let raw_id = payload["book_id"].as_i64().unwrap_or(0) as i32;
    if raw_id > 0 {
        let exists = book::Entity::find_by_id(raw_id).one(db).await?;
        if exists.is_some() {
            return Ok(Some(raw_id));
        }
    }

    Ok(None)
}

// ── Copy handlers ────────────────────────────────────────────────────

async fn apply_copy_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    // Resolve book_id via ISBN lookup (cross-device safe)
    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            tracing::info!(
                "Skipping copy create: referenced book not found locally (op #{})",
                op.id
            );
            return Ok(());
        }
    };

    let status = payload["status"]
        .as_str()
        .unwrap_or("available")
        .to_string();
    let is_temporary = payload["is_temporary"].as_bool().unwrap_or(false);

    // Deduplication: skip if a copy with same (book_id, status, is_temporary) already exists
    let existing = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq(status.clone()))
        .filter(copy::Column::IsTemporary.eq(is_temporary))
        .one(db)
        .await?;
    if existing.is_some() {
        tracing::info!("Skipping duplicate copy for book_id={book_id}");
        return Ok(());
    }

    // Always resolve library_id locally (source device's ID may not exist here)
    let lib_id = crate::utils::library_helpers::resolve_library_id(db).await?;

    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(lib_id),
        status: Set(status),
        notes: Set(payload["notes"].as_str().map(|s| s.to_string())),
        is_temporary: Set(is_temporary),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    copy::Entity::insert(new_copy).exec(db).await?;
    Ok(())
}

async fn apply_copy_update(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let existing = copy::Entity::find_by_id(op.entity_id).one(db).await?;
    if let Some(c) = existing {
        let payload = parse_payload(op)?;
        let mut active: copy::ActiveModel = c.into();
        if let Some(s) = payload.get("status").and_then(|v| v.as_str()) {
            active.status = Set(s.to_string());
        }
        if let Some(n) = payload.get("notes").and_then(|v| v.as_str()) {
            active.notes = Set(Some(n.to_string()));
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.save(db).await?;
    }
    Ok(())
}

// ── Contact handlers ─────────────────────────────────────────────────

async fn apply_contact_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    // Deduplication: skip if contact with same name already exists
    let name = payload["name"].as_str().unwrap_or("Unknown").to_string();
    let existing = contact::Entity::find()
        .filter(contact::Column::Name.eq(name.clone()))
        .one(db)
        .await?;
    if existing.is_some() {
        tracing::info!("⏭️ Skipping duplicate contact: {name}");
        return Ok(());
    }

    let new_contact = contact::ActiveModel {
        r#type: Set(payload["type"].as_str().unwrap_or("Person").to_string()),
        name: Set(name),
        first_name: Set(payload["first_name"].as_str().map(|s| s.to_string())),
        email: Set(payload["email"].as_str().map(|s| s.to_string())),
        phone: Set(payload["phone"].as_str().map(|s| s.to_string())),
        notes: Set(payload["notes"].as_str().map(|s| s.to_string())),
        library_owner_id: Set(
            match payload["library_owner_id"].as_i64().map(|v| v as i32) {
                Some(id) => id,
                None => crate::utils::library_helpers::resolve_library_id(db).await?,
            },
        ),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    contact::Entity::insert(new_contact).exec(db).await?;
    Ok(())
}

async fn apply_contact_update(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let existing = contact::Entity::find_by_id(op.entity_id).one(db).await?;
    if let Some(c) = existing {
        let payload = parse_payload(op)?;
        let mut active: contact::ActiveModel = c.into();
        if let Some(n) = payload.get("name").and_then(|v| v.as_str()) {
            active.name = Set(n.to_string());
        }
        if let Some(e) = payload.get("email").and_then(|v| v.as_str()) {
            active.email = Set(Some(e.to_string()));
        }
        if let Some(p) = payload.get("phone").and_then(|v| v.as_str()) {
            active.phone = Set(Some(p.to_string()));
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.save(db).await?;
    }
    Ok(())
}

// ── Loan handlers ────────────────────────────────────────────────────

async fn apply_loan_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    let new_loan = loan::ActiveModel {
        copy_id: Set(payload["copy_id"].as_i64().unwrap_or(0) as i32),
        contact_id: Set(payload["contact_id"].as_i64().unwrap_or(0) as i32),
        library_id: Set(match payload["library_id"].as_i64().map(|v| v as i32) {
            Some(id) => id,
            None => crate::utils::library_helpers::resolve_library_id(db).await?,
        }),
        loan_date: Set(payload["loan_date"].as_str().unwrap_or(&now).to_string()),
        due_date: Set(payload["due_date"].as_str().unwrap_or(&now).to_string()),
        status: Set(payload["status"].as_str().unwrap_or("active").to_string()),
        notes: Set(payload["notes"].as_str().map(|s| s.to_string())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    loan::Entity::insert(new_loan).exec(db).await?;
    Ok(())
}

async fn apply_loan_update(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let existing = loan::Entity::find_by_id(op.entity_id).one(db).await?;
    if let Some(l) = existing {
        let payload = parse_payload(op)?;
        let mut active: loan::ActiveModel = l.into();
        if let Some(s) = payload.get("status").and_then(|v| v.as_str()) {
            active.status = Set(s.to_string());
        }
        if let Some(rd) = payload.get("return_date").and_then(|v| v.as_str()) {
            active.return_date = Set(Some(rd.to_string()));
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.save(db).await?;
    }
    Ok(())
}

// ── Tag handler ──────────────────────────────────────────────────────

async fn apply_tag_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let name = payload["name"].as_str().unwrap_or("Unknown").to_string();
    let now = chrono::Utc::now().to_rfc3339();

    // Deduplication: skip if tag with same name already exists
    let existing = tag::Entity::find()
        .filter(tag::Column::Name.eq(name.clone()))
        .one(db)
        .await?;
    if existing.is_some() {
        tracing::info!("⏭️ Skipping duplicate tag: {name}");
        return Ok(());
    }

    let new_tag = tag::ActiveModel {
        name: Set(name.clone()),
        parent_id: Set(payload["parent_id"].as_i64().map(|v| v as i32)),
        path: Set(payload["path"].as_str().unwrap_or(&name).to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    tag::Entity::insert(new_tag).exec(db).await?;
    Ok(())
}

// ── Author handler ───────────────────────────────────────────────────

async fn apply_author_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    // Deduplication: skip if author with same name already exists
    let name = payload["name"].as_str().unwrap_or("Unknown").to_string();
    let existing = author::Entity::find()
        .filter(author::Column::Name.eq(name.clone()))
        .one(db)
        .await?;
    if existing.is_some() {
        tracing::info!("⏭️ Skipping duplicate author: {name}");
        return Ok(());
    }

    let new_author = author::ActiveModel {
        name: Set(name),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    author::Entity::insert(new_author).exec(db).await?;
    Ok(())
}

// ── Junction table helpers (resolved by natural keys) ─────────────────

async fn apply_book_author_insert(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    // Resolve book by ISBN/title (cross-device safe)
    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            // Fallback: raw book_id for same-device backfill
            let raw = payload["book_id"].as_i64().unwrap_or(0) as i32;
            if raw > 0 && book::Entity::find_by_id(raw).one(db).await?.is_some() {
                raw
            } else {
                return Ok(());
            }
        }
    };

    // Resolve author by name (cross-device safe) or raw ID fallback
    let author_id = if let Some(name) = payload
        .get("author_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        find_or_create_author(db, name, &now).await?
    } else {
        let raw = payload["author_id"].as_i64().unwrap_or(0) as i32;
        if raw > 0 && author::Entity::find_by_id(raw).one(db).await?.is_some() {
            raw
        } else {
            return Ok(());
        }
    };

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT OR IGNORE INTO book_authors (book_id, author_id) VALUES ($1, $2)",
        [book_id.into(), author_id.into()],
    ))
    .await?;
    Ok(())
}

async fn apply_book_author_delete(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;

    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            let raw = payload["book_id"].as_i64().unwrap_or(0) as i32;
            if raw > 0 {
                raw
            } else {
                return Ok(());
            }
        }
    };

    let author_id = if let Some(name) = payload
        .get("author_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        match author::Entity::find()
            .filter(author::Column::Name.eq(name))
            .one(db)
            .await?
        {
            Some(a) => a.id,
            None => return Ok(()),
        }
    } else {
        payload["author_id"].as_i64().unwrap_or(0) as i32
    };

    if book_id > 0 && author_id > 0 {
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "DELETE FROM book_authors WHERE book_id = $1 AND author_id = $2",
            [book_id.into(), author_id.into()],
        ))
        .await?;
    }
    Ok(())
}

async fn apply_book_tag_insert(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            let raw = payload["book_id"].as_i64().unwrap_or(0) as i32;
            if raw > 0 && book::Entity::find_by_id(raw).one(db).await?.is_some() {
                raw
            } else {
                return Ok(());
            }
        }
    };

    let tag_id = if let Some(name) = payload
        .get("tag_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        find_or_create_tag(db, name, &now).await?
    } else {
        let raw = payload["tag_id"].as_i64().unwrap_or(0) as i32;
        if raw > 0 && tag::Entity::find_by_id(raw).one(db).await?.is_some() {
            raw
        } else {
            return Ok(());
        }
    };

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "INSERT OR IGNORE INTO book_tags (book_id, tag_id) VALUES ($1, $2)",
        [book_id.into(), tag_id.into()],
    ))
    .await?;
    Ok(())
}

async fn apply_book_tag_delete(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;

    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            let raw = payload["book_id"].as_i64().unwrap_or(0) as i32;
            if raw > 0 {
                raw
            } else {
                return Ok(());
            }
        }
    };

    let tag_id = if let Some(name) = payload
        .get("tag_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        match tag::Entity::find()
            .filter(tag::Column::Name.eq(name))
            .one(db)
            .await?
        {
            Some(t) => t.id,
            None => return Ok(()),
        }
    } else {
        payload["tag_id"].as_i64().unwrap_or(0) as i32
    };

    if book_id > 0 && tag_id > 0 {
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "DELETE FROM book_tags WHERE book_id = $1 AND tag_id = $2",
            [book_id.into(), tag_id.into()],
        ))
        .await?;
    }
    Ok(())
}

// ── Collection handlers (string UUID IDs) ────────────────────────────

async fn apply_collection_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let str_id = payload["_str_id"]
        .as_str()
        .unwrap_or(&uuid::Uuid::new_v4().to_string())
        .to_string();

    // Deduplication: skip if collection with same UUID already exists
    if collection::Entity::find_by_id(str_id.clone())
        .one(db)
        .await?
        .is_some()
    {
        tracing::info!("Skipping duplicate collection: {str_id}");
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    let new_collection = collection::ActiveModel {
        id: Set(str_id),
        name: Set(payload["name"].as_str().unwrap_or("Collection").to_string()),
        description: Set(payload["description"].as_str().map(|s| s.to_string())),
        source: Set(payload["source"].as_str().unwrap_or("user").to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    };
    collection::Entity::insert(new_collection).exec(db).await?;
    Ok(())
}

async fn apply_collection_delete(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let str_id = payload["_str_id"].as_str().unwrap_or("");
    if !str_id.is_empty() {
        collection::Entity::delete_by_id(str_id.to_string())
            .exec(db)
            .await?;
    }
    Ok(())
}

async fn apply_collection_book_insert(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let collection_id = payload["_str_id"]
        .as_str()
        .or(payload["collection_id"].as_str())
        .unwrap_or("")
        .to_string();

    if collection_id.is_empty() {
        return Ok(());
    }

    // Resolve book_id via ISBN/title (cross-device safe)
    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            // Fallback: try raw book_id for same-device backfill
            let raw = payload["book_id"].as_i64().unwrap_or(0) as i32;
            if raw > 0 && book::Entity::find_by_id(raw).one(db).await?.is_some() {
                raw
            } else {
                tracing::info!(
                    "Skipping collection_book: book not found locally (op #{})",
                    op.id
                );
                return Ok(());
            }
        }
    };

    // Verify collection exists locally
    if collection::Entity::find_by_id(collection_id.clone())
        .one(db)
        .await?
        .is_none()
    {
        tracing::info!("Skipping collection_book: collection {collection_id} not found locally");
        return Ok(());
    }

    let entry = collection_book::ActiveModel {
        collection_id: Set(collection_id),
        book_id: Set(book_id),
        added_at: Set(chrono::Utc::now().to_rfc3339()),
    };
    let _ = collection_book::Entity::insert(entry).exec(db).await;
    Ok(())
}

async fn apply_collection_book_delete(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let collection_id = payload["_str_id"]
        .as_str()
        .or(payload["collection_id"].as_str())
        .unwrap_or("");

    if collection_id.is_empty() {
        return Ok(());
    }

    // Resolve book_id via ISBN/title (cross-device safe)
    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            let raw = payload["book_id"].as_i64().unwrap_or(0) as i32;
            if raw > 0 {
                raw
            } else {
                return Ok(());
            }
        }
    };

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "DELETE FROM collection_books WHERE collection_id = $1 AND book_id = $2",
        [collection_id.into(), book_id.into()],
    ))
    .await?;
    Ok(())
}

// ── Book note handlers (device sync only) ───────────────────────────

async fn apply_book_note_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    use crate::modules::book_notes::models as bn;

    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    // Resolve book_id via ISBN lookup (cross-device safe)
    let book_id = match resolve_local_book_id(db, &payload).await? {
        Some(id) => id,
        None => {
            tracing::info!(
                "Skipping book_note create: referenced book not found locally (op #{})",
                op.id
            );
            return Ok(());
        }
    };

    let content = payload["content"].as_str().unwrap_or("").to_string();
    let page = payload["page"].as_i64().map(|v| v as i32);

    if content.is_empty() {
        return Err(DbErr::Custom("book_note: empty content".to_string()));
    }

    // Deduplication: skip if an identical note already exists for this book
    let mut dedup_query = bn::Entity::find()
        .filter(bn::Column::BookId.eq(book_id))
        .filter(bn::Column::Content.eq(content.clone()));
    if let Some(p) = page {
        dedup_query = dedup_query.filter(bn::Column::Page.eq(p));
    } else {
        dedup_query = dedup_query.filter(bn::Column::Page.is_null());
    }
    if dedup_query.one(db).await?.is_some() {
        tracing::info!("Skipping duplicate book_note for book_id={book_id}");
        return Ok(());
    }

    let note = bn::ActiveModel {
        book_id: Set(book_id),
        content: Set(content),
        page: Set(page),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    bn::Entity::insert(note).exec(db).await?;
    Ok(())
}

async fn apply_book_note_update(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    use crate::modules::book_notes::models as bn;

    let existing = bn::Entity::find_by_id(op.entity_id).one(db).await?;
    if let Some(n) = existing {
        let payload = parse_payload(op)?;
        let mut active: bn::ActiveModel = n.into();
        if let Some(c) = payload.get("content").and_then(|v| v.as_str()) {
            active.content = Set(c.to_string());
        }
        if payload.get("page").is_some() {
            active.page = Set(payload["page"].as_i64().map(|v| v as i32));
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.save(db).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;
    use sea_orm::{ActiveModelTrait, EntityTrait, ModelTrait, Set};

    /// Helper: insert a pending operation from a remote device.
    async fn insert_remote_op(
        db: &DatabaseConnection,
        entity_type: &str,
        entity_id: i32,
        operation: &str,
        payload: serde_json::Value,
    ) -> operation_log::Model {
        let op = operation_log::ActiveModel {
            entity_type: Set(entity_type.to_owned()),
            entity_id: Set(entity_id),
            operation: Set(operation.to_owned()),
            payload: Set(Some(payload.to_string())),
            status: Set("pending".to_owned()),
            source: Set("device:test".to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        op.insert(db).await.expect("Failed to insert op")
    }

    #[tokio::test]
    async fn test_apply_book_create_operation() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        let payload = serde_json::json!({
            "title": "Test Book",
            "isbn": "TEST-123",
            "authors": "Test Author"
        });

        let op = insert_remote_op(&db, "book", 1, "create", payload).await;

        process_next_batch(&db).await.expect("Processing failed");

        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .expect("DB error")
            .unwrap();

        assert_eq!(
            updated_op.status, "applied",
            "Operation should be applied. Error: {:?}",
            updated_op.error_message
        );

        let book = book::Entity::find()
            .filter(book::Column::Isbn.eq("TEST-123"))
            .one(&db)
            .await
            .expect("DB error");

        assert!(book.is_some(), "Book should be created");
        assert_eq!(book.unwrap().title, "Test Book");
    }

    #[tokio::test]
    async fn test_copy_created_via_isbn_resolution() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        // Pre-existing local book (different ID than source device)
        let local_book = book::ActiveModel {
            title: Set("Existing Book".to_string()),
            isbn: Set(Some("ISBN-CROSS-DEVICE".to_string())),
            owned: Set(true),
            reading_status: Set("to_read".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_book = local_book.insert(&db).await.unwrap();

        // Insert copy operation with book_isbn (from another device, book_id=999 does NOT exist locally)
        let payload = serde_json::json!({
            "book_id": 999,
            "book_isbn": "ISBN-CROSS-DEVICE",
            "status": "available",
            "is_temporary": false,
        });
        let op = insert_remote_op(&db, "copy", 50, "insert", payload).await;

        process_next_batch(&db).await.expect("Processing failed");

        // Verify operation applied
        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated_op.status, "applied",
            "Copy op should be applied. Error: {:?}",
            updated_op.error_message
        );

        // Verify copy was created with the LOCAL book_id
        let copies = copy::Entity::find()
            .filter(copy::Column::BookId.eq(local_book.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(copies.len(), 1, "One copy should exist for the local book");
        assert_eq!(copies[0].status, "available");
    }

    #[tokio::test]
    async fn test_copy_skipped_when_book_not_found() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        // Copy with ISBN that does NOT match any local book
        let payload = serde_json::json!({
            "book_id": 999,
            "book_isbn": "ISBN-DOES-NOT-EXIST",
            "status": "available",
            "is_temporary": false,
        });
        let op = insert_remote_op(&db, "copy", 50, "insert", payload).await;

        process_next_batch(&db).await.expect("Processing failed");

        // Operation should be applied (gracefully skipped, not failed)
        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_op.status, "applied");

        // No copies created
        let all_copies = copy::Entity::find().all(&db).await.unwrap();
        assert!(all_copies.is_empty());
    }

    #[tokio::test]
    async fn test_book_note_created_via_isbn_resolution() {
        use crate::modules::book_notes::models as bn;

        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        // Pre-existing local book
        let local_book = book::ActiveModel {
            title: Set("Note Target".to_string()),
            isbn: Set(Some("ISBN-NOTE-SYNC".to_string())),
            owned: Set(true),
            reading_status: Set("reading".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_book = local_book.insert(&db).await.unwrap();

        // Insert book_note operation with book_isbn
        let payload = serde_json::json!({
            "book_id": 888,
            "book_isbn": "ISBN-NOTE-SYNC",
            "content": "Great chapter on page 10",
            "page": 10,
        });
        let op = insert_remote_op(&db, "book_note", 30, "insert", payload).await;

        process_next_batch(&db).await.expect("Processing failed");

        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated_op.status, "applied",
            "book_note op should be applied. Error: {:?}",
            updated_op.error_message
        );

        // Verify note was created with the LOCAL book_id
        let notes = bn::Entity::find()
            .filter(bn::Column::BookId.eq(local_book.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Great chapter on page 10");
        assert_eq!(notes[0].page, Some(10));
    }

    #[tokio::test]
    async fn test_duplicate_copy_skipped_on_second_sync() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        let local_book = book::ActiveModel {
            title: Set("Dedup Book".to_string()),
            isbn: Set(Some("ISBN-DEDUP-COPY".to_string())),
            owned: Set(true),
            reading_status: Set("to_read".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_book = local_book.insert(&db).await.unwrap();

        let payload = serde_json::json!({
            "book_id": 999,
            "book_isbn": "ISBN-DEDUP-COPY",
            "status": "available",
            "is_temporary": false,
        });

        // First sync: copy created
        insert_remote_op(&db, "copy", 50, "insert", payload.clone()).await;
        process_next_batch(&db).await.unwrap();

        // Second sync: same copy operation again
        insert_remote_op(&db, "copy", 50, "insert", payload).await;
        process_next_batch(&db).await.unwrap();

        // Only ONE copy should exist
        let copies = copy::Entity::find()
            .filter(copy::Column::BookId.eq(local_book.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(copies.len(), 1, "Duplicate copy should be skipped");
    }

    #[tokio::test]
    async fn test_apply_book_create_with_authors_and_copy() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        let payload = serde_json::json!({
            "title": "Le Petit Prince",
            "isbn": "978-2070612758",
            "authors": ["Antoine de Saint-Exupery"],
            "tags": ["fiction", "classique"],
            "subjects": ["fiction", "classique", "jeunesse"],
            "owned": true,
            "reading_status": "read",
        });

        let op = insert_remote_op(&db, "book", 1, "insert", payload).await;
        process_next_batch(&db).await.expect("Processing failed");

        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated_op.status, "applied",
            "Op should be applied. Error: {:?}",
            updated_op.error_message
        );

        // Book created
        let book_model = book::Entity::find()
            .filter(book::Column::Isbn.eq("978-2070612758"))
            .one(&db)
            .await
            .unwrap()
            .expect("Book should exist");

        // Author created and linked
        let authors: Vec<author::Model> = book_model
            .find_related(author::Entity)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(authors.len(), 1);
        assert_eq!(authors[0].name, "Antoine de Saint-Exupery");
        // Verify timestamps are set (was the bug: missing created_at/updated_at)
        assert!(!authors[0].created_at.is_empty());
        assert!(!authors[0].updated_at.is_empty());

        // Tags created and linked (via book_tags junction)
        let tags: Vec<tag::Model> = book_model.find_related(tag::Entity).all(&db).await.unwrap();
        assert_eq!(tags.len(), 2);
        let tag_names: Vec<&str> = tags.iter().map(|t| t.name.as_str()).collect();
        assert!(tag_names.contains(&"fiction"));
        assert!(tag_names.contains(&"classique"));

        // Subjects (shelf assignments) restored in book JSON column
        assert!(
            book_model.subjects.is_some(),
            "Subjects should be set on the synced book"
        );
        let subjects: Vec<String> =
            serde_json::from_str(book_model.subjects.as_ref().unwrap()).unwrap();
        assert_eq!(subjects.len(), 3);
        assert!(subjects.contains(&"jeunesse".to_string()));

        // Default copy created for owned book
        let copies = copy::Entity::find()
            .filter(copy::Column::BookId.eq(book_model.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            copies.len(),
            1,
            "Default copy should be created for owned book"
        );
        assert_eq!(copies[0].status, "available");
    }

    #[tokio::test]
    async fn test_copy_resolved_by_title_when_no_isbn() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        // Book without ISBN
        let local_book = book::ActiveModel {
            title: Set("Mon Livre Sans ISBN".to_string()),
            isbn: Set(None),
            owned: Set(true),
            reading_status: Set("to_read".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_book = local_book.insert(&db).await.unwrap();

        // Copy with book_title fallback (no ISBN)
        let payload = serde_json::json!({
            "book_id": 999,
            "book_isbn": null,
            "book_title": "Mon Livre Sans ISBN",
            "status": "available",
            "is_temporary": false,
        });
        let op = insert_remote_op(&db, "copy", 50, "insert", payload).await;
        process_next_batch(&db).await.expect("Processing failed");

        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated_op.status, "applied",
            "Copy should be applied via title fallback. Error: {:?}",
            updated_op.error_message
        );

        let copies = copy::Entity::find()
            .filter(copy::Column::BookId.eq(local_book.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(copies.len(), 1, "Copy resolved by title should exist");
    }

    #[tokio::test]
    async fn test_book_author_junction_resolved_by_natural_keys() {
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        // Create a local book and author
        let local_book = book::ActiveModel {
            title: Set("Junction Test".to_string()),
            isbn: Set(Some("ISBN-JUNCTION".to_string())),
            owned: Set(true),
            reading_status: Set("to_read".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_book = local_book.insert(&db).await.unwrap();

        let local_author = author::ActiveModel {
            name: Set("Junction Author".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_author = local_author.insert(&db).await.unwrap();

        // Simulate junction op from another device (different IDs, but natural keys match)
        let payload = serde_json::json!({
            "book_id": 999,
            "author_id": 888,
            "book_isbn": "ISBN-JUNCTION",
            "book_title": "Junction Test",
            "author_name": "Junction Author",
        });
        let op = insert_remote_op(&db, "book_author", 0, "insert", payload).await;
        process_next_batch(&db).await.unwrap();

        let updated_op = operation_log::Entity::find_by_id(op.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_op.status, "applied");

        // Verify junction created with LOCAL IDs
        let linked_authors: Vec<author::Model> = local_book
            .find_related(author::Entity)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(linked_authors.len(), 1);
        assert_eq!(linked_authors[0].id, local_author.id);
    }

    #[tokio::test]
    async fn test_duplicate_book_note_skipped_on_second_sync() {
        use crate::modules::book_notes::models as bn;

        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        let local_book = book::ActiveModel {
            title: Set("Dedup Note Book".to_string()),
            isbn: Set(Some("ISBN-DEDUP-NOTE".to_string())),
            owned: Set(true),
            reading_status: Set("reading".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let local_book = local_book.insert(&db).await.unwrap();

        let payload = serde_json::json!({
            "book_id": 888,
            "book_isbn": "ISBN-DEDUP-NOTE",
            "content": "Same note twice",
            "page": 5,
        });

        // First sync: note created
        insert_remote_op(&db, "book_note", 30, "insert", payload.clone()).await;
        process_next_batch(&db).await.unwrap();

        // Second sync: same note again
        insert_remote_op(&db, "book_note", 30, "insert", payload).await;
        process_next_batch(&db).await.unwrap();

        // Only ONE note should exist
        let notes = bn::Entity::find()
            .filter(bn::Column::BookId.eq(local_book.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(notes.len(), 1, "Duplicate book_note should be skipped");
    }
}
