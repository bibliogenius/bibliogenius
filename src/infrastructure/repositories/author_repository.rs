//! SeaORM implementation of AuthorRepository

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};

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

    async fn find_by_id(&self, id: i32) -> Result<Option<Author>, DomainError> {
        let author = AuthorEntity::find_by_id(id).one(&self.db).await?;

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

    async fn delete(&self, id: i32) -> Result<(), DomainError> {
        let result = AuthorEntity::delete_by_id(id).exec(&self.db).await?;

        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(())
    }
}
