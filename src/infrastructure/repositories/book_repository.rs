//! SeaORM implementation of BookRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, Set,
};

use crate::domain::{BookFilter, BookRepository, DomainError, PaginatedBooks};
use crate::models::Book;
use crate::models::book::{ActiveModel, Column, Entity as BookEntity};

/// SeaORM-based implementation of BookRepository
pub struct SeaOrmBookRepository {
    db: DatabaseConnection,
}

impl SeaOrmBookRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl BookRepository for SeaOrmBookRepository {
    async fn find_all(&self, filter: BookFilter) -> Result<PaginatedBooks, DomainError> {
        let mut query = BookEntity::find();

        // Apply filters
        if let Some(status) = &filter.status
            && !status.is_empty()
        {
            query = query.filter(Column::ReadingStatus.eq(status));
        }

        if let Some(title) = &filter.title
            && !title.is_empty()
        {
            query = query.filter(Column::Title.contains(title));
        }

        if let Some(tag) = &filter.tag
            && !tag.is_empty()
        {
            query = query.filter(Column::Subjects.contains(tag));
        }

        if let Some(q) = &filter.query
            && !q.is_empty()
        {
            use sea_orm::sea_query::Expr;
            let cond = Condition::any()
                .add(Column::Title.contains(q))
                .add(Column::Isbn.contains(q))
                .add(Column::Subjects.contains(q))
                .add(Expr::col(Column::Id).in_subquery(Book::author_search_subquery(q)));
            query = query.filter(cond);
        }

        // Filter owned books only (used by peer sync to exclude borrowed books)
        if filter.owned_only == Some(true) {
            query = query.filter(Column::Owned.eq(true));
        }

        // Apply sorting
        match filter.sort.as_deref() {
            Some("title_asc") => query = query.order_by_asc(Column::Title),
            Some("title_desc") => query = query.order_by_desc(Column::Title),
            Some("recent") => query = query.order_by_desc(Column::CreatedAt),
            _ => query = query.order_by_asc(Column::ShelfPosition),
        }

        // Fetch with pagination and total count
        let (books, total) = if let Some(limit) = filter.limit {
            let page = filter.page.unwrap_or(0);
            let paginator = query.paginate(&self.db, limit);
            let total = paginator.num_items().await.unwrap_or(0);
            let items = paginator.fetch_page(page).await?;
            (items, total)
        } else {
            let items = query.all(&self.db).await?;
            let total = items.len() as u64;
            (items, total)
        };

        // Convert to DTOs and fetch related authors
        let book_dtos = Book::populate_authors(&self.db, books).await;

        Ok(PaginatedBooks {
            books: book_dtos,
            total,
        })
    }

    async fn find_by_id(&self, id: i32) -> Result<Option<Book>, DomainError> {
        let book_model = BookEntity::find_by_id(id).one(&self.db).await?;

        match book_model {
            Some(model) => {
                let mut dtos = Book::populate_authors(&self.db, vec![model]).await;
                Ok(dtos.pop())
            }
            None => Ok(None),
        }
    }

    async fn create(&self, book: Book) -> Result<Book, DomainError> {
        let now = chrono::Utc::now();

        let subjects_json = book
            .subjects
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

        let digital_formats_json = book
            .digital_formats
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

        let reading_status = book
            .reading_status
            .clone()
            .unwrap_or_else(|| "to_read".to_string());
        let owned = book.owned.unwrap_or_else(|| reading_status != "wanting");

        let new_book = ActiveModel {
            title: Set(book.title.clone()),
            isbn: Set(book.isbn),
            summary: Set(book.summary),
            publisher: Set(book.publisher),
            publication_year: Set(book.publication_year),
            dewey_decimal: Set(book.dewey_decimal),
            lcc: Set(book.lcc),
            cover_url: Set(book.cover_url),
            subjects: Set(subjects_json),
            marc_record: Set(book.marc_record),
            cataloguing_notes: Set(book.cataloguing_notes),
            reading_status: Set(reading_status),
            shelf_position: Set(book.shelf_position),
            user_rating: Set(book.user_rating),
            owned: Set(owned),
            price: Set(book.price),
            digital_formats: Set(digital_formats_json),
            source_data: Set(book.source_data),
            finished_reading_at: Set(book.finished_reading_at.flatten()),
            started_reading_at: Set(book.started_reading_at.flatten()),
            created_at: Set(now.to_rfc3339()),
            updated_at: Set(now.to_rfc3339()),
            ..Default::default()
        };

        let result = new_book.insert(&self.db).await?;
        Ok(Book::from(result))
    }

    async fn update(&self, id: i32, book: Book) -> Result<Book, DomainError> {
        let existing = BookEntity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(DomainError::NotFound)?;

        let now = chrono::Utc::now();

        let subjects_json = book
            .subjects
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

        let digital_formats_json = book
            .digital_formats
            .as_ref()
            .map(|s| serde_json::to_string(s).unwrap_or_else(|_| "[]".to_string()));

        let mut active: ActiveModel = existing.into();
        active.title = Set(book.title);
        active.isbn = Set(book.isbn);
        active.summary = Set(book.summary);
        active.publisher = Set(book.publisher);
        active.publication_year = Set(book.publication_year);
        active.dewey_decimal = Set(book.dewey_decimal);
        active.lcc = Set(book.lcc);
        active.cover_url = Set(book.cover_url);
        active.subjects = Set(subjects_json);
        active.marc_record = Set(book.marc_record);
        active.cataloguing_notes = Set(book.cataloguing_notes);
        active.reading_status = Set(book.reading_status.unwrap_or_else(|| "to_read".to_string()));
        active.shelf_position = Set(book.shelf_position);
        active.user_rating = Set(book.user_rating);
        active.owned = Set(book.owned.unwrap_or(true));
        active.price = Set(book.price);
        active.digital_formats = Set(digital_formats_json);
        active.finished_reading_at = Set(book.finished_reading_at.flatten());
        active.started_reading_at = Set(book.started_reading_at.flatten());
        active.updated_at = Set(now.to_rfc3339());

        let result = active.update(&self.db).await?;
        Ok(Book::from(result))
    }

    async fn delete(&self, id: i32) -> Result<(), DomainError> {
        let result = BookEntity::delete_by_id(id).exec(&self.db).await?;

        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(())
    }

    async fn find_missing_covers(&self) -> Result<Vec<(i32, String)>, DomainError> {
        let models = BookEntity::find()
            .filter(Column::CoverUrl.is_null())
            .filter(Column::Isbn.is_not_null())
            .all(&self.db)
            .await?;

        Ok(models
            .into_iter()
            .filter_map(|m| {
                let isbn = m.isbn.as_deref().unwrap_or("").trim().to_string();
                if isbn.is_empty() {
                    None
                } else {
                    Some((m.id, isbn))
                }
            })
            .collect())
    }

    async fn update_cover_url(&self, id: i32, cover_url: &str) -> Result<(), DomainError> {
        BookEntity::update_many()
            .col_expr(
                Column::CoverUrl,
                sea_orm::sea_query::Expr::value(cover_url.to_string()),
            )
            .filter(Column::Id.eq(id))
            .exec(&self.db)
            .await?;

        Ok(())
    }
}
