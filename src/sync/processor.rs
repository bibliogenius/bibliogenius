use crate::models::{book, operation_log};
use sea_orm::*;
use serde_json::Value;
use std::time::Duration;

pub async fn run_processor(db: DatabaseConnection) {
    tracing::info!("🔄 Operation Processor started");

    loop {
        match process_next_batch(&db).await { Err(e) => {
            tracing::error!("❌ Error processing operations: {}", e);
            tokio::time::sleep(Duration::from_secs(5)).await;
        } _ => {
            // If no operations were found, verify inside process_next_batch if we should sleep
            // For simplicity here, we assume process_next_batch handles the sleep if empty
        }}
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
        ("book", "create") | ("book", "insert") => apply_book_create(&txn, &op).await,
        ("book", "update") => apply_book_update(&txn, &op).await,
        // Add more handlers here
        _ => {
            tracing::warn!(
                "⚠️ Unknown operation type: {} {}",
                op.entity_type,
                op.operation
            );
            Ok(()) // Mark as applied (or skipped) to avoid stuck loop
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
