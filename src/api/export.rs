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
