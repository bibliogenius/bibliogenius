//! SeaORM implementation of BookNoteRepository.

use async_trait::async_trait;
use sea_orm::*;

use super::domain::{BookNote, BookNoteRepository, CreateBookNoteInput, UpdateBookNoteInput};
use super::models;
use crate::domain::DomainError;

pub struct SeaOrmBookNoteRepository {
    db: DatabaseConnection,
}

impl SeaOrmBookNoteRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

fn model_to_domain(m: models::Model) -> BookNote {
    BookNote {
        id: m.id,
        book_id: m.book_id,
        content: m.content,
        page: m.page,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

#[async_trait]
impl BookNoteRepository for SeaOrmBookNoteRepository {
    async fn find_by_book_id(&self, book_id: i32) -> Result<Vec<BookNote>, DomainError> {
        let notes = models::Entity::find()
            .filter(models::Column::BookId.eq(book_id))
            .order_by_desc(models::Column::CreatedAt)
            .all(&self.db)
            .await?;
        Ok(notes.into_iter().map(model_to_domain).collect())
    }

    async fn find_by_id(&self, id: i32) -> Result<Option<BookNote>, DomainError> {
        let note = models::Entity::find_by_id(id).one(&self.db).await?;
        Ok(note.map(model_to_domain))
    }

    async fn create(
        &self,
        book_id: i32,
        input: CreateBookNoteInput,
    ) -> Result<BookNote, DomainError> {
        let now = chrono::Utc::now().to_rfc3339();
        let active = models::ActiveModel {
            book_id: Set(book_id),
            content: Set(input.content),
            page: Set(input.page),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        let result = models::Entity::insert(active).exec(&self.db).await?;
        self.find_by_id(result.last_insert_id)
            .await?
            .ok_or(DomainError::Internal(
                "Failed to read back created note".to_string(),
            ))
    }

    async fn update(&self, id: i32, input: UpdateBookNoteInput) -> Result<BookNote, DomainError> {
        let existing = models::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(DomainError::NotFound)?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut active: models::ActiveModel = existing.into();
        active.content = Set(input.content);
        active.page = Set(input.page);
        active.updated_at = Set(now);
        active.update(&self.db).await?;
        self.find_by_id(id).await?.ok_or(DomainError::Internal(
            "Failed to read back updated note".to_string(),
        ))
    }

    async fn delete(&self, id: i32) -> Result<(), DomainError> {
        let result = models::Entity::delete_by_id(id).exec(&self.db).await?;
        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }
}
