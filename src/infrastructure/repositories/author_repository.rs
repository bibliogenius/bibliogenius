//! SeaORM implementation of AuthorRepository

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};

use crate::domain::{Author, AuthorRepository, DomainError};
use crate::models::author::{ActiveModel, Entity as AuthorEntity};

/// SeaORM-based implementation of AuthorRepository
pub struct SeaOrmAuthorRepository {
    db: DatabaseConnection,
}

impl SeaOrmAuthorRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl AuthorRepository for SeaOrmAuthorRepository {
    async fn find_all(&self) -> Result<Vec<Author>, DomainError> {
        let authors = AuthorEntity::find().all(&self.db).await?;

        Ok(authors
            .into_iter()
            .map(|a| Author {
                id: a.id,
                name: a.name,
                created_at: a.created_at,
                updated_at: a.updated_at,
            })
            .collect())
    }

    async fn find_by_id(&self, id: &str) -> Result<Option<Author>, DomainError> {
        let author = AuthorEntity::find_by_id(id.to_owned())
            .one(&self.db)
            .await?;

        Ok(author.map(|a| Author {
            id: a.id,
            name: a.name,
            created_at: a.created_at,
            updated_at: a.updated_at,
        }))
    }

    async fn create(&self, name: String) -> Result<Author, DomainError> {
        let now = chrono::Utc::now().to_rfc3339();

        let author = ActiveModel {
            name: Set(name),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };

        let result = author.insert(&self.db).await?;

        Ok(Author {
            id: result.id,
            name: result.name,
            created_at: result.created_at,
            updated_at: result.updated_at,
        })
    }

    async fn delete(&self, id: &str) -> Result<(), DomainError> {
        // Cascade the author's book links in one transaction: the database no
        // longer does it since the replicated tables lost their foreign keys
        // (ADR-044). Roll back when the author does not exist so a not-found
        // delete leaves the data untouched.
        let txn = self.db.begin().await?;
        let existed =
            crate::infrastructure::referential_integrity::delete_author_cascade(&txn, id).await?;
        if !existed {
            txn.rollback().await?;
            return Err(DomainError::NotFound);
        }
        txn.commit().await?;

        Ok(())
    }
}
