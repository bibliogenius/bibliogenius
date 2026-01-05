use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use sea_orm::{DatabaseConnection, EntityTrait};
use serde::Serialize;

use crate::models::{book, contact, copy, library_config, loan, peer, tag};

#[derive(Serialize)]
pub struct BackupData {
    pub version: String,
    pub timestamp: String,
    pub library_config: Option<library_config::Model>,
    pub books: Vec<book::Model>,
    pub copies: Vec<copy::Model>,
    pub contacts: Vec<contact::Model>,
    pub loans: Vec<loan::Model>,
    pub peers: Vec<peer::Model>,
    pub tags: Vec<tag::Model>,
}

pub async fn export_data(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // Fetch all data
    let config = library_config::Entity::find_by_id(1)
        .one(&db)
        .await
        .unwrap_or(None);
    let books = book::Entity::find().all(&db).await.unwrap_or_default();
    let copies = copy::Entity::find().all(&db).await.unwrap_or_default();
    let contacts = contact::Entity::find().all(&db).await.unwrap_or_default();
    let loans = loan::Entity::find().all(&db).await.unwrap_or_default();
    let peers = peer::Entity::find().all(&db).await.unwrap_or_default();
    let tags = tag::Entity::find().all(&db).await.unwrap_or_default();

    let backup = BackupData {
        version: "1.0".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        library_config: config,
        books,
        copies,
        contacts,
        loans,
        peers,
        tags,
    };

    let filename = format!(
        "bibliogenius_backup_{}.json",
        chrono::Utc::now().format("%Y-%m-%d")
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", filename)
            .parse()
            .unwrap(),
    );

    (StatusCode::OK, headers, Json(backup))
}

// --- Import Backup ---

use serde::Deserialize;

#[derive(Deserialize)]
pub struct ImportBackupData {
    pub version: Option<String>,
    pub timestamp: Option<String>,
    pub library_config: Option<library_config::Model>,
    pub books: Option<Vec<book::Model>>,
    pub copies: Option<Vec<copy::Model>>,
    pub contacts: Option<Vec<contact::Model>>,
    pub loans: Option<Vec<loan::Model>>,
    pub peers: Option<Vec<peer::Model>>,
    pub tags: Option<Vec<tag::Model>>,
}

#[derive(Serialize)]
pub struct ImportResult {
    pub success: bool,
    pub books_imported: usize,
    pub copies_imported: usize,
    pub contacts_imported: usize,
    pub loans_imported: usize,
    pub tags_imported: usize,
    pub message: String,
}

pub async fn import_data(
    State(db): State<DatabaseConnection>,
    Json(backup): Json<ImportBackupData>,
) -> impl IntoResponse {
    use sea_orm::{ActiveModelTrait, IntoActiveModel};

    let mut books_count = 0;
    let mut copies_count = 0;
    let mut contacts_count = 0;
    let mut loans_count = 0;
    let mut tags_count = 0;

    // Import books
    if let Some(books) = backup.books {
        for book in books {
            let active = book.into_active_model();
            // Try insert, if conflict on primary key, do nothing (already exists)
            match active.insert(&db).await {
                Ok(_) => books_count += 1,
                Err(_) => {} // Already exists, skip
            }
        }
    }

    // Import copies
    if let Some(copies) = backup.copies {
        for copy in copies {
            let active = copy.into_active_model();
            match active.insert(&db).await {
                Ok(_) => copies_count += 1,
                Err(_) => {}
            }
        }
    }

    // Import contacts
    if let Some(contacts) = backup.contacts {
        for contact in contacts {
            let active = contact.into_active_model();
            match active.insert(&db).await {
                Ok(_) => contacts_count += 1,
                Err(_) => {}
            }
        }
    }

    // Import loans
    if let Some(loans) = backup.loans {
        for loan in loans {
            let active = loan.into_active_model();
            match active.insert(&db).await {
                Ok(_) => loans_count += 1,
                Err(_) => {}
            }
        }
    }

    // Import tags
    if let Some(tags) = backup.tags {
        for tag in tags {
            let active = tag.into_active_model();
            match active.insert(&db).await {
                Ok(_) => tags_count += 1,
                Err(_) => {}
            }
        }
    }

    let total = books_count + copies_count + contacts_count + loans_count + tags_count;
    let result = ImportResult {
        success: true,
        books_imported: books_count,
        copies_imported: copies_count,
        contacts_imported: contacts_count,
        loans_imported: loans_count,
        tags_imported: tags_count,
        message: format!("Successfully imported {} items", total),
    };

    (StatusCode::OK, Json(result))
}
