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

    /// Count books in a collection that have `owned = true`.
    async fn count_owned_books(&self, collection_id: &str) -> i64 {
        use crate::models::book;
        CollectionBookEntity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .join(JoinType::InnerJoin, collection_book::Relation::Book.def())
            .filter(book::Column::Owned.eq(true))
            .count(&self.db)
            .await
            .unwrap_or(0) as i64
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

            let owned = self.count_owned_books(&col.id).await;

            result.push(Collection {
                id: col.id,
                name: col.name,
                description: col.description,
                source: col.source,
                created_at: col.created_at,
                updated_at: col.updated_at,
                total_books: total,
                owned_books: owned,
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

                let owned = self.count_owned_books(&col.id).await;

                Ok(Some(Collection {
                    id: col.id,
                    name: col.name,
                    description: col.description,
                    source: col.source,
                    created_at: col.created_at,
                    updated_at: col.updated_at,
                    total_books: total,
                    owned_books: owned,
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
        // Get all collection_book entries for this collection, in reading order:
        // numbered volumes ascending, then unnumbered (NULL) by `added_at`. The
        // SQLite `ORDER BY <nullable> ASC` default places NULLs first, which is the
        // opposite of what the frise wants, so the ordering is applied in Rust
        // below where NULL is explicitly ranked last.
        let mut collection_books = CollectionBookEntity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .all(&self.db)
            .await?;

        collection_books.sort_by(|a, b| {
            // Unnumbered (None) sorts after any numbered volume, then by insertion
            // time. Compared by reference so the comparator allocates nothing.
            (a.volume_number.is_none(), a.volume_number)
                .cmp(&(b.volume_number.is_none(), b.volume_number))
                .then_with(|| a.added_at.cmp(&b.added_at))
        });

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
                    reading_status: Some(book.reading_status),
                    volume_number: cb.volume_number,
                });
            }
        }

        Ok(result)
    }

    async fn set_source(&self, id: &str, source: &str) -> Result<(), DomainError> {
        let existing = CollectionEntity::find_by_id(id).one(&self.db).await?;
        let Some(model) = existing else {
            return Err(DomainError::NotFound);
        };

        let mut active: ActiveModel = model.into();
        active.source = Set(source.to_owned());
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(&self.db).await?;
        Ok(())
    }

    async fn set_book_volume(
        &self,
        collection_id: &str,
        book_id: &str,
        volume_number: Option<i32>,
    ) -> Result<(), DomainError> {
        let existing = CollectionBookEntity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .filter(collection_book::Column::BookId.eq(book_id))
            .one(&self.db)
            .await?;

        // No-op if the book is not a member: the caller adds it first.
        if let Some(model) = existing {
            let mut active: CollectionBookActiveModel = model.into();
            active.volume_number = Set(volume_number);
            active.update(&self.db).await?;
        }
        Ok(())
    }

    async fn add_book(&self, collection_id: &str, book_id: &str) -> Result<(), DomainError> {
        // Check if already exists
        let existing = CollectionBookEntity::find()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .filter(collection_book::Column::BookId.eq(book_id))
            .one(&self.db)
            .await?;

        if existing.is_some() {
            return Ok(()); // Already exists, idempotent
        }

        // Create new entry (unnumbered; a volume number is assigned separately
        // for series-typed collections via `set_book_volume`).
        let new_entry = CollectionBookActiveModel {
            collection_id: Set(collection_id.to_string()),
            book_id: Set(book_id.to_owned()),
            added_at: Set(chrono::Utc::now().to_rfc3339()),
            volume_number: Set(None),
        };

        new_entry.insert(&self.db).await?;
        Ok(())
    }

    async fn remove_book(&self, collection_id: &str, book_id: &str) -> Result<(), DomainError> {
        collection_book::Entity::delete_many()
            .filter(collection_book::Column::CollectionId.eq(collection_id))
            .filter(collection_book::Column::BookId.eq(book_id))
            .exec(&self.db)
            .await?;

        Ok(())
    }

    async fn get_book_collections(&self, book_id: &str) -> Result<Vec<Collection>, DomainError> {
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
        book_id: &str,
        collection_ids: Vec<String>,
    ) -> Result<(), DomainError> {
        // Preserve any series ordering across the replace: this handler backs the
        // book-detail collection chip picker, which knows nothing about volume
        // numbers, so a naive delete-then-reinsert would silently wipe them. Snapshot
        // the existing (collection -> volume_number) before deleting and restore it
        // for collections the book stays in.
        let previous: std::collections::HashMap<String, Option<i32>> = CollectionBookEntity::find()
            .filter(collection_book::Column::BookId.eq(book_id))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|cb| (cb.collection_id, cb.volume_number))
            .collect();

        // 1. Remove existing associations
        collection_book::Entity::delete_many()
            .filter(collection_book::Column::BookId.eq(book_id))
            .exec(&self.db)
            .await?;

        // 2. Add new associations, carrying forward the prior volume number.
        let now = chrono::Utc::now().to_rfc3339();
        for col_id in collection_ids {
            let volume_number = previous.get(&col_id).copied().flatten();
            let new_entry = CollectionBookActiveModel {
                collection_id: Set(col_id),
                book_id: Set(book_id.to_owned()),
                added_at: Set(now.clone()),
                volume_number: Set(volume_number),
            };
            new_entry.insert(&self.db).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CreateCollectionInput;
    use crate::models::book;
    use sea_orm::{ConnectionTrait, Set, Statement};

    async fn setup() -> (DatabaseConnection, SeaOrmCollectionRepository) {
        let db = crate::db::init_db("sqlite::memory:").await.unwrap();
        // Seeded books use `library_id = 0`, so relax the FK checks like the
        // other repository/service tests do.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();
        let repo = SeaOrmCollectionRepository::new(db.clone());
        (db, repo)
    }

    /// Insert a book with an explicit uuid PK (`Entity::insert` skips `before_save`),
    /// returning its id. `reading_status` and `owned` are set for the frise assertions.
    async fn insert_book(
        db: &DatabaseConnection,
        title: &str,
        reading_status: &str,
        owned: bool,
    ) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let id = crate::utils::uuid_gen::new_uuid_v7();
        book::Entity::insert(book::ActiveModel {
            id: Set(id.clone()),
            title: Set(title.to_owned()),
            reading_status: Set(reading_status.to_owned()),
            owned: Set(owned),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        })
        .exec(db)
        .await
        .unwrap();
        id
    }

    async fn make_collection(repo: &SeaOrmCollectionRepository, name: &str) -> String {
        repo.create(CreateCollectionInput {
            name: name.to_owned(),
            description: None,
            source: None,
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn get_books_orders_numbered_first_then_unnumbered() {
        let (db, repo) = setup().await;
        let col = make_collection(&repo, "Cycle").await;
        let b_none = insert_book(&db, "Unnumbered", "to_read", true).await;
        let b3 = insert_book(&db, "Tome 3", "to_read", true).await;
        let b1 = insert_book(&db, "Tome 1", "read", true).await;

        // Add out of order; assign volumes to two of the three.
        repo.add_book(&col, &b_none).await.unwrap();
        repo.add_book(&col, &b3).await.unwrap();
        repo.add_book(&col, &b1).await.unwrap();
        repo.set_book_volume(&col, &b3, Some(3)).await.unwrap();
        repo.set_book_volume(&col, &b1, Some(1)).await.unwrap();

        let books = repo.get_books(&col).await.unwrap();
        let ids: Vec<&str> = books.iter().map(|b| b.book_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![b1.as_str(), b3.as_str(), b_none.as_str()],
            "numbered volumes ascending, unnumbered last"
        );
        assert_eq!(books[0].volume_number, Some(1));
        assert_eq!(books[2].volume_number, None);
        // reading_status is surfaced for the frise dimming.
        assert_eq!(books[0].reading_status.as_deref(), Some("read"));
        assert_eq!(books[1].reading_status.as_deref(), Some("to_read"));
    }

    #[tokio::test]
    async fn set_book_volume_can_clear_back_to_null() {
        let (db, repo) = setup().await;
        let col = make_collection(&repo, "Cycle").await;
        let b = insert_book(&db, "Tome", "to_read", true).await;
        repo.add_book(&col, &b).await.unwrap();

        repo.set_book_volume(&col, &b, Some(2)).await.unwrap();
        assert_eq!(
            repo.get_books(&col).await.unwrap()[0].volume_number,
            Some(2)
        );

        repo.set_book_volume(&col, &b, None).await.unwrap();
        assert_eq!(repo.get_books(&col).await.unwrap()[0].volume_number, None);
    }

    #[tokio::test]
    async fn set_book_volume_is_noop_for_non_member() {
        let (db, repo) = setup().await;
        let col = make_collection(&repo, "Cycle").await;
        let b = insert_book(&db, "Tome", "to_read", true).await;
        // Not added to the collection: must not error and must not create a row.
        repo.set_book_volume(&col, &b, Some(1)).await.unwrap();
        assert!(repo.get_books(&col).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn set_source_flips_collection_to_series() {
        let (_db, repo) = setup().await;
        let col = make_collection(&repo, "Cycle").await;
        assert_eq!(
            repo.find_by_id(&col).await.unwrap().unwrap().source,
            "manual"
        );

        repo.set_source(&col, "series").await.unwrap();
        assert_eq!(
            repo.find_by_id(&col).await.unwrap().unwrap().source,
            "series"
        );
    }

    #[tokio::test]
    async fn update_book_collections_preserves_volume_for_retained_collections() {
        let (db, repo) = setup().await;
        let series = make_collection(&repo, "Cycle").await;
        let other = make_collection(&repo, "To read").await;
        let b = insert_book(&db, "Tome 2", "to_read", true).await;

        repo.add_book(&series, &b).await.unwrap();
        repo.set_book_volume(&series, &b, Some(2)).await.unwrap();

        // The book-detail chip picker replaces the membership set, keeping the
        // series and adding another collection. The volume number must survive.
        repo.update_book_collections(&b, vec![series.clone(), other.clone()])
            .await
            .unwrap();

        assert_eq!(
            repo.get_books(&series).await.unwrap()[0].volume_number,
            Some(2),
            "volume number preserved for a retained collection"
        );
        assert_eq!(
            repo.get_books(&other).await.unwrap()[0].volume_number,
            None,
            "a newly-added collection starts unnumbered"
        );
    }
}
