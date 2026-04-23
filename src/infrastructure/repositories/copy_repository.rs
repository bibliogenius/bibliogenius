//! SeaORM implementation of CopyRepository

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::domain::{
    Copy, CopyRepository, CreateCopyInput, DomainError, PaginatedCopies, UpdateCopyInput,
};
use crate::models::book::{self, Entity as BookEntity};
use crate::models::copy::{self, ActiveModel, Column, Entity as CopyEntity};

/// Single source of truth for `copy::Model` -> domain `Copy` mapping.
/// Takes the optional joined book row so callers that do `find_also_related`
/// and those that don't can share the same field list.
fn to_domain(copy: copy::Model, book: Option<book::Model>) -> Copy {
    Copy {
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
        lender_display_name: copy.lender_display_name,
        lender_peer_id: copy.lender_peer_id,
        borrow_due_date: copy.borrow_due_date,
        borrow_source: copy.borrow_source,
    }
}

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
            .map(|(copy, book)| to_domain(copy, book))
            .collect();

        let total = copies.len();
        Ok(PaginatedCopies { copies, total })
    }

    async fn find_by_id(&self, id: i32) -> Result<Option<Copy>, DomainError> {
        let result = CopyEntity::find_by_id(id)
            .find_also_related(BookEntity)
            .one(&self.db)
            .await?;

        Ok(result.map(|(copy, book)| to_domain(copy, book)))
    }

    async fn find_by_book_id(&self, book_id: i32) -> Result<PaginatedCopies, DomainError> {
        let copies = CopyEntity::find()
            .filter(Column::BookId.eq(book_id))
            .all(&self.db)
            .await?;

        let copies: Vec<Copy> = copies
            .into_iter()
            .map(|copy| to_domain(copy, None))
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
            .map(|(copy, book)| to_domain(copy, book))
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
            lender_display_name: Set(input.lender_display_name),
            lender_peer_id: Set(input.lender_peer_id),
            borrow_due_date: Set(input.borrow_due_date),
            borrow_source: Set(input.borrow_source),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };

        let result = new_copy.insert(&self.db).await?;
        Ok(to_domain(result, None))
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
        if let Some(name) = input.lender_display_name {
            active.lender_display_name = Set(name);
        }
        if let Some(peer_id) = input.lender_peer_id {
            active.lender_peer_id = Set(peer_id);
        }
        if let Some(due) = input.borrow_due_date {
            active.borrow_due_date = Set(due);
        }
        if let Some(source) = input.borrow_source {
            active.borrow_source = Set(source);
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());

        let result = active.update(&self.db).await?;
        Ok(to_domain(result, None))
    }

    async fn delete(&self, id: i32) -> Result<(), DomainError> {
        let result = CopyEntity::delete_by_id(id).exec(&self.db).await?;

        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(())
    }
}
