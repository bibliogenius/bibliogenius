//! SeaORM implementation of CopyRepository

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::domain::{
    Copy, CopyRepository, CreateCopyInput, DomainError, PaginatedCopies, UpdateCopyInput,
};
use crate::models::book::Entity as BookEntity;
use crate::models::copy::{ActiveModel, Column, Entity as CopyEntity};

/// SeaORM-based implementation of CopyRepository
pub struct SeaOrmCopyRepository {
    db: DatabaseConnection,
}

impl SeaOrmCopyRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl CopyRepository for SeaOrmCopyRepository {
    async fn find_all(&self) -> Result<PaginatedCopies, DomainError> {
        let copies_with_books = CopyEntity::find()
            .find_also_related(BookEntity)
            .all(&self.db)
            .await?;

        let copies: Vec<Copy> = copies_with_books
            .into_iter()
            .map(|(copy, book)| Copy {
                id: Some(copy.id),
                book_id: copy.book_id,
                library_id: copy.library_id,
                acquisition_date: copy.acquisition_date,
                notes: copy.notes,
                status: copy.status,
                is_temporary: copy.is_temporary,
                book_title: book.as_ref().map(|b| b.title.clone()),
                book_cover: book.and_then(|b| b.cover_url),
                price: copy.price,
                sold_at: copy.sold_at,
            })
            .collect();

        let total = copies.len();
        Ok(PaginatedCopies { copies, total })
    }

    async fn find_by_id(&self, id: i32) -> Result<Option<Copy>, DomainError> {
        let result = CopyEntity::find_by_id(id)
            .find_also_related(BookEntity)
            .one(&self.db)
            .await?;

        Ok(result.map(|(copy, book)| Copy {
            id: Some(copy.id),
            book_id: copy.book_id,
            library_id: copy.library_id,
            acquisition_date: copy.acquisition_date,
            notes: copy.notes,
            status: copy.status,
            is_temporary: copy.is_temporary,
            book_title: book.as_ref().map(|b| b.title.clone()),
            book_cover: book.and_then(|b| b.cover_url),
            price: copy.price,
            sold_at: copy.sold_at,
        }))
    }

    async fn find_by_book_id(&self, book_id: i32) -> Result<PaginatedCopies, DomainError> {
        let copies = CopyEntity::find()
            .filter(Column::BookId.eq(book_id))
            .all(&self.db)
            .await?;

        let copies: Vec<Copy> = copies
            .into_iter()
            .map(|copy| Copy {
                id: Some(copy.id),
                book_id: copy.book_id,
                library_id: copy.library_id,
                acquisition_date: copy.acquisition_date,
                notes: copy.notes,
                status: copy.status,
                is_temporary: copy.is_temporary,
                book_title: None,
                book_cover: None,
                price: copy.price,
                sold_at: copy.sold_at,
            })
            .collect();

        let total = copies.len();
        Ok(PaginatedCopies { copies, total })
    }

    async fn find_borrowed(&self) -> Result<PaginatedCopies, DomainError> {
        let copies_with_books = CopyEntity::find()
            .filter(Column::IsTemporary.eq(true))
            .find_also_related(BookEntity)
            .all(&self.db)
            .await?;

        let copies: Vec<Copy> = copies_with_books
            .into_iter()
            .map(|(copy, book)| Copy {
                id: Some(copy.id),
                book_id: copy.book_id,
                library_id: copy.library_id,
                acquisition_date: copy.acquisition_date,
                notes: copy.notes,
                status: copy.status,
                is_temporary: copy.is_temporary,
                book_title: book.as_ref().map(|b| b.title.clone()),
                book_cover: book.and_then(|b| b.cover_url),
                price: copy.price,
                sold_at: copy.sold_at,
            })
            .collect();

        let total = copies.len();
        Ok(PaginatedCopies { copies, total })
    }

    async fn create(&self, input: CreateCopyInput) -> Result<Copy, DomainError> {
        let now = chrono::Utc::now().to_rfc3339();

        let new_copy = ActiveModel {
            book_id: Set(input.book_id),
            library_id: Set(input.library_id),
            acquisition_date: Set(input.acquisition_date),
            notes: Set(input.notes),
            status: Set(input.status),
            is_temporary: Set(input.is_temporary),
            price: Set(input.price),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };

        let result = new_copy.insert(&self.db).await?;

        Ok(Copy {
            id: Some(result.id),
            book_id: result.book_id,
            library_id: result.library_id,
            acquisition_date: result.acquisition_date,
            notes: result.notes,
            status: result.status,
            is_temporary: result.is_temporary,
            book_title: None,
            book_cover: None,
            price: result.price,
            sold_at: result.sold_at,
        })
    }

    async fn update(&self, id: i32, input: UpdateCopyInput) -> Result<Copy, DomainError> {
        let existing = CopyEntity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(DomainError::NotFound)?;

        let mut active: ActiveModel = existing.into();

        if let Some(status) = input.status {
            active.status = Set(status);
        }
        if let Some(notes) = input.notes {
            active.notes = Set(notes);
        }
        if let Some(date) = input.acquisition_date {
            active.acquisition_date = Set(date);
        }
        if let Some(price) = input.price {
            active.price = Set(price);
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());

        let result = active.update(&self.db).await?;

        Ok(Copy {
            id: Some(result.id),
            book_id: result.book_id,
            library_id: result.library_id,
            acquisition_date: result.acquisition_date,
            notes: result.notes,
            status: result.status,
            is_temporary: result.is_temporary,
            book_title: None,
            book_cover: None,
            price: result.price,
            sold_at: result.sold_at,
        })
    }

    async fn delete(&self, id: i32) -> Result<(), DomainError> {
        let result = CopyEntity::delete_by_id(id).exec(&self.db).await?;

        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(())
    }
}
