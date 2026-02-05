//! SeaORM implementation of BookRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, ModelTrait,
    PaginatorTrait, QueryFilter, QueryOrder, Set,
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
            let cond = Condition::any()
                .add(Column::Title.contains(q))
                .add(Column::Isbn.contains(q))
                .add(Column::Subjects.contains(q));
            query = query.filter(cond);
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
        let mut book_dtos = Vec::with_capacity(books.len());
        for book_model in books {
            let mut book_dto = Book::from(book_model.clone());

            // Fetch authors
            if let Ok(authors) = book_model
                .find_related(crate::models::author::Entity)
                .all(&self.db)
                .await
                && !authors.is_empty()
            {
                let author_names: Vec<String> = authors.into_iter().map(|a| a.name).collect();
                book_dto.author = Some(author_names.join(", "));
                book_dto.authors = Some(author_names);
            }

            // Derive cover_url from ISBN
            if let Some(isbn) = &book_dto.isbn {
                book_dto.cover_url = Some(format!(
                    "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                    isbn
                ));
            }

            book_dtos.push(book_dto);
        }

        Ok(PaginatedBooks {
            books: book_dtos,
            total,
        })
    }

    async fn find_by_id(&self, id: i32) -> Result<Option<Book>, DomainError> {
        let book_model = BookEntity::find_by_id(id).one(&self.db).await?;

        match book_model {
            Some(model) => {
                let mut book_dto = Book::from(model.clone());

                // Fetch authors
                if let Ok(authors) = model
                    .find_related(crate::models::author::Entity)
                    .all(&self.db)
                    .await
                    && !authors.is_empty()
                {
                    let author_names: Vec<String> = authors.into_iter().map(|a| a.name).collect();
                    book_dto.author = Some(author_names.join(", "));
                    book_dto.authors = Some(author_names);
                }

                if let Some(isbn) = &book_dto.isbn {
                    book_dto.cover_url = Some(format!(
                        "https://covers.openlibrary.org/b/isbn/{}-M.jpg",
                        isbn
                    ));
                }

                Ok(Some(book_dto))
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
}
