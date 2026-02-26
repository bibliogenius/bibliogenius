use crate::models::{
    author, book, book_authors, book_tags, collection, collection_book, contact, copy, loan,
    operation_log, tag,
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
    // Fetch one pending operation (FIFO)
    let pending_op = operation_log::Entity::find()
        .filter(operation_log::Column::Status.eq("pending"))
        .order_by_asc(operation_log::Column::CreatedAt)
        .one(db)
        .await?;

    match pending_op {
        Some(op) => {
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
        // Book-Author / Book-Tag junction tables
        ("book_author", "insert") => apply_junction_insert::<book_authors::Entity>(&txn, &op).await,
        ("book_author", "delete") => apply_junction_delete::<book_authors::Entity>(&txn, &op).await,
        ("book_tag", "insert") => apply_junction_insert::<book_tags::Entity>(&txn, &op).await,
        ("book_tag", "delete") => apply_junction_delete::<book_tags::Entity>(&txn, &op).await,
        // Collections (string UUID IDs)
        ("collection", "insert") => apply_collection_create(&txn, &op).await,
        ("collection", "delete") => apply_collection_delete(&txn, &op).await,
        ("collection_book", "insert") => apply_collection_book_insert(&txn, &op).await,
        ("collection_book", "delete") => apply_collection_book_delete(&txn, &op).await,
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

    // Check if book already exists (Idempotency)
    // Note: In a real P2P system, we rely on UUIDs. Here we might use ISBN or a shared ID.
    // For specific P2P logic, we assume payload contains the full book data.

    // Simplification: We blindly insert/update based on ID if provided, or ignore if ID mismatch.
    // Ideally we deserialize into book::Model

    let title = payload["title"].as_str().unwrap_or("Unknown").to_string();
    let isbn = payload["isbn"].as_str().map(|s| s.to_string());

    let new_book = book::ActiveModel {
        title: Set(title),
        isbn: Set(isbn),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    book::Entity::insert(new_book).exec(db).await?;

    Ok(())
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

// ── Copy handlers ────────────────────────────────────────────────────

async fn apply_copy_create(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr> {
    let payload = parse_payload(op)?;
    let now = chrono::Utc::now().to_rfc3339();

    let new_copy = copy::ActiveModel {
        book_id: Set(payload["book_id"].as_i64().unwrap_or(0) as i32),
        library_id: Set(payload["library_id"].as_i64().unwrap_or(1) as i32),
        status: Set(payload["status"]
            .as_str()
            .unwrap_or("available")
            .to_string()),
        notes: Set(payload["notes"].as_str().map(|s| s.to_string())),
        is_temporary: Set(payload["is_temporary"].as_bool().unwrap_or(false)),
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

    let new_contact = contact::ActiveModel {
        r#type: Set(payload["type"].as_str().unwrap_or("Person").to_string()),
        name: Set(payload["name"].as_str().unwrap_or("Unknown").to_string()),
        first_name: Set(payload["first_name"].as_str().map(|s| s.to_string())),
        email: Set(payload["email"].as_str().map(|s| s.to_string())),
        phone: Set(payload["phone"].as_str().map(|s| s.to_string())),
        notes: Set(payload["notes"].as_str().map(|s| s.to_string())),
        library_owner_id: Set(payload["library_owner_id"].as_i64().unwrap_or(1) as i32),
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
        library_id: Set(payload["library_id"].as_i64().unwrap_or(1) as i32),
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

    let new_author = author::ActiveModel {
        name: Set(payload["name"].as_str().unwrap_or("Unknown").to_string()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };
    author::Entity::insert(new_author).exec(db).await?;
    Ok(())
}

// ── Junction table helpers (book_authors, book_tags) ─────────────────

async fn apply_junction_insert<E>(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr>
where
    E: EntityTrait,
{
    let payload = parse_payload(op)?;
    let book_id = payload["book_id"].as_i64().unwrap_or(0) as i32;
    let related_id = payload
        .get("author_id")
        .or(payload.get("tag_id"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    // Use raw SQL for junction tables since they have composite PKs
    let table = E::default().table_name().to_string();
    let col_name = if table.contains("author") {
        "author_id"
    } else {
        "tag_id"
    };

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        format!("INSERT OR IGNORE INTO {table} (book_id, {col_name}) VALUES ($1, $2)"),
        [book_id.into(), related_id.into()],
    ))
    .await?;

    Ok(())
}

async fn apply_junction_delete<E>(
    db: &DatabaseTransaction,
    op: &operation_log::Model,
) -> Result<(), DbErr>
where
    E: EntityTrait,
{
    let payload = parse_payload(op)?;
    let book_id = payload["book_id"].as_i64().unwrap_or(0) as i32;
    let related_id = payload
        .get("author_id")
        .or(payload.get("tag_id"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let table = E::default().table_name().to_string();
    let col_name = if table.contains("author") {
        "author_id"
    } else {
        "tag_id"
    };

    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        format!("DELETE FROM {table} WHERE book_id = $1 AND {col_name} = $2"),
        [book_id.into(), related_id.into()],
    ))
    .await?;

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
    let book_id = payload["book_id"].as_i64().unwrap_or(0) as i32;

    if !collection_id.is_empty() && book_id > 0 {
        let entry = collection_book::ActiveModel {
            collection_id: Set(collection_id),
            book_id: Set(book_id),
            added_at: Set(chrono::Utc::now().to_rfc3339()),
        };
        // Use insert or ignore to handle duplicates
        let _ = collection_book::Entity::insert(entry).exec(db).await;
    }
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
    let book_id = payload["book_id"].as_i64().unwrap_or(0) as i32;

    if !collection_id.is_empty() && book_id > 0 {
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "DELETE FROM collection_books WHERE collection_id = $1 AND book_id = $2",
            [collection_id.into(), book_id.into()],
        ))
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    #[tokio::test]
    async fn test_apply_book_create_operation() {
        // 1. Setup in-memory DB
        let db = init_db("sqlite::memory:").await.expect("Failed to init db");

        // 2. Insert a pending operation
        let payload = serde_json::json!({
            "title": "Test Book",
            "isbn": "TEST-123",
            "authors": "Test Author"
        });

        let op = operation_log::ActiveModel {
            entity_type: Set("book".to_owned()),
            entity_id: Set(1),
            operation: Set("create".to_owned()),
            payload: Set(Some(payload.to_string())),
            status: Set("pending".to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let op = op.insert(&db).await.expect("Failed to insert op");

        // 3. Process the batch (call private function)
        process_next_batch(&db).await.expect("Processing failed");

        // 4. Verify Side Effects
        // Check Operation Status updated first to see if it failed
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

        // Check Book created
        let book = book::Entity::find()
            .filter(book::Column::Isbn.eq("TEST-123"))
            .one(&db)
            .await
            .expect("DB error");

        assert!(book.is_some(), "Book should be created");
        let book = book.unwrap();
        assert_eq!(book.title, "Test Book");
    }
}
