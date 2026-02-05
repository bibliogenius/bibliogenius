//! Application state containing repositories and shared resources

use sea_orm::DatabaseConnection;
use std::sync::Arc;

use crate::domain::{AuthorRepository, BookRepository, CollectionRepository, CopyRepository};
use crate::infrastructure::{
    SeaOrmAuthorRepository, SeaOrmBookRepository, SeaOrmCollectionRepository, SeaOrmCopyRepository,
};

/// Application state shared across all handlers
#[derive(Clone)]
pub struct AppState {
    /// Database connection (for backward compatibility)
    db: DatabaseConnection,
    /// Book repository
    pub book_repo: Arc<dyn BookRepository>,
    /// Author repository
    pub author_repo: Arc<dyn AuthorRepository>,
    /// Copy repository
    pub copy_repo: Arc<dyn CopyRepository>,
    /// Collection repository
    pub collection_repo: Arc<dyn CollectionRepository>,
}

impl AppState {
    /// Create a new AppState with all repositories initialized
    pub fn new(db: DatabaseConnection) -> Self {
        let book_repo = Arc::new(SeaOrmBookRepository::new(db.clone()));
        let author_repo = Arc::new(SeaOrmAuthorRepository::new(db.clone()));
        let copy_repo = Arc::new(SeaOrmCopyRepository::new(db.clone()));
        let collection_repo = Arc::new(SeaOrmCollectionRepository::new(db.clone()));

        Self {
            db,
            book_repo,
            author_repo,
            copy_repo,
            collection_repo,
        }
    }

    /// Get the database connection (for backward compatibility during migration)
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }
}

// Allow extracting DatabaseConnection from AppState for backward compatibility
impl AsRef<DatabaseConnection> for AppState {
    fn as_ref(&self) -> &DatabaseConnection {
        &self.db
    }
}

// Implement FromRef to allow extracting DatabaseConnection from AppState
impl axum::extract::FromRef<AppState> for DatabaseConnection {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}
