//! SeaORM implementation of CollectionRepository

use async_trait::async_trait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, JoinType, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, RelationTrait, Set,
};
use uuid::Uuid;

use crate::domain::{
    Collection, CollectionBook, CollectionRepository, CreateCollectionInput, DomainError,
};
use crate::models::book::Entity as BookEntity;
use crate::models::collection::{ActiveModel, Column, Entity as CollectionEntity};
use crate::models::collection_book::{
    self, ActiveModel as CollectionBookActiveModel, Entity as CollectionBookEntity,
};

/// SeaORM-based implementation of CollectionRepository
pub struct SeaOrmCollectionRepository {
    db: DatabaseConnection,
}

impl SeaOrmCollectionRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl CollectionRepository for SeaOrmCollectionRepository {
    async fn find_all(&self) -> Result<Vec<Collection>, DomainError> {
        let collections = CollectionEntity::find()
            .order_by_desc(Column::CreatedAt)
            .all(&self.db)
            .await?;

        let mut result = Vec::new();
        for col in collections {
            // Count total books in collection
            let total = CollectionBookEntity::find()
                .filter(collection_book::Column::CollectionId.eq(&col.id))
                .count(&self.db)
                .await
                .unwrap_or(0) as i64;

            result.push(Collection {
                id: col.id,
                name: col.name,
                description: col.description,
                source: col.source,
                created_at: col.created_at,
                updated_at: col.updated_at,
                total_books: total,
                owned_books: total, // For now, same as total
            });
        }

        Ok(result)
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<Collection>, DomainError> {
        let result = CollectionEntity::find_by_id(id).one(&self.db).await?;

        match result {
            Some(col) => {
                let total = CollectionBookEntity::find()
                    .filter(collection_book::Column::CollectionId.eq(&col.id))
                    .count(&self.db)
                    .await
                    .unwrap_or(0) as i64;

                Ok(Some(Collection {
                    id: col.id,
                    name: col.name,
                    description: col.description,
                    source: col.source,
                    created_at: col.created_at,
                    updated_at: col.updated_at,
                    total_books: total,
                    owned_books: total,
                }))
            }
            None => Ok(None),
        }
    }

    async fn create(&self, input: CreateCollectionInput) -> Result<Collection, DomainError> {
        let now = chrono::Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();

        let new_collection = ActiveModel {
            id: Set(id.clone()),
            name: Set(input.name.clone()),
            description: Set(input.description.clone()),
            source: Set(input.source.unwrap_or_else(|| "manual".to_string())),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
        };

        let result = new_collection.insert(&self.db).await?;

        Ok(Collection {
            id: result.id,
            name: result.name,
            description: result.description,
            source: result.source,
            created_at: result.created_at,
            updated_at: result.updated_at,
            total_books: 0,
            owned_books: 0,
        })
    }

    async fn delete(&self, id: &str) -> Result<(), DomainError> {
        let result = CollectionEntity::delete_by_id(id).exec(&self.db).await?;

        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(())
    }

    async fn get_books(&self, collection_id: &str) -> Result<Vec<CollectionBook>, DomainError> {
        // Get all collection_book entries for this collection
        let collection_books = CollectionBookEntity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .all(&self.db)
            .await?;

        let mut result = Vec::new();
        for cb in collection_books {
            // Fetch book details for each (N+1 query for now, optimization later)
            if let Some(book) = BookEntity::find_by_id(cb.book_id).one(&self.db).await? {
                result.push(CollectionBook {
                    book_id: book.id,
                    title: book.title,
                    author: None, // TODO: Join with authors table
                    cover_url: book.cover_url,
                    publisher: book.publisher,
                    publication_year: book.publication_year,
                    added_at: cb.added_at,
                    is_owned: book.owned,
                    digital_formats: book
                        .digital_formats
                        .and_then(|s| serde_json::from_str(&s).ok()),
                });
            }
        }

        Ok(result)
    }

    async fn add_book(&self, collection_id: &str, book_id: i32) -> Result<(), DomainError> {
        // Check if already exists
        let existing = CollectionBookEntity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .filter(collection_book::Column::BookId.eq(book_id))
            .one(&self.db)
            .await?;

        if existing.is_some() {
            return Ok(()); // Already exists, idempotent
        }

        // Create new entry
        let new_entry = CollectionBookActiveModel {
            collection_id: Set(collection_id.to_string()),
            book_id: Set(book_id),
            added_at: Set(chrono::Utc::now().to_rfc3339()),
        };

        new_entry.insert(&self.db).await?;
        Ok(())
    }

    async fn remove_book(&self, collection_id: &str, book_id: i32) -> Result<(), DomainError> {
        collection_book::Entity::delete_many()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .filter(collection_book::Column::BookId.eq(book_id))
            .exec(&self.db)
            .await?;

        Ok(())
    }

    async fn get_book_collections(&self, book_id: i32) -> Result<Vec<Collection>, DomainError> {
        let collections = CollectionEntity::find()
            .join(
                JoinType::InnerJoin,
                collection_book::Relation::Collection.def().rev(),
            )
            .filter(collection_book::Column::BookId.eq(book_id))
            .all(&self.db)
            .await?;

        let result = collections
            .into_iter()
            .map(|col| Collection {
                id: col.id,
                name: col.name,
                description: col.description,
                source: col.source,
                created_at: col.created_at,
                updated_at: col.updated_at,
                total_books: 0, // Not needed for this view
                owned_books: 0,
            })
            .collect();

        Ok(result)
    }

    async fn update_book_collections(
        &self,
        book_id: i32,
        collection_ids: Vec<String>,
    ) -> Result<(), DomainError> {
        // 1. Remove existing associations
        collection_book::Entity::delete_many()
            .filter(collection_book::Column::BookId.eq(book_id))
            .exec(&self.db)
            .await?;

        // 2. Add new associations
        let now = chrono::Utc::now().to_rfc3339();
        for col_id in collection_ids {
            let new_entry = CollectionBookActiveModel {
                collection_id: Set(col_id),
                book_id: Set(book_id),
                added_at: Set(now.clone()),
            };
            new_entry.insert(&self.db).await?;
        }

        Ok(())
    }
}
