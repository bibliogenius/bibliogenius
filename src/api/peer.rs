use crate::models::{operation_log, peer};
use axum::{
    extract::{State, Json},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Deserialize)]
pub struct ConnectRequest {
    name: String,
    url: String,
    public_key: Option<String>,
}

pub async fn connect(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<ConnectRequest>,
) -> impl IntoResponse {
    let peer = peer::ActiveModel {
        name: Set(payload.name),
        url: Set(payload.url),
        public_key: Set(payload.public_key),
        last_seen: Set(Some(chrono::Utc::now().to_rfc3339())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match peer::Entity::insert(peer).exec(&db).await {
        Ok(res) => (StatusCode::CREATED, Json(json!({ "id": res.last_insert_id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct PushRequest {
    operations: Vec<OperationDto>,
}

#[derive(Serialize, Deserialize)]
pub struct OperationDto {
    entity_type: String,
    entity_id: i32,
    operation: String,
    payload: Option<String>,
    created_at: String,
}

pub async fn push_operations(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<PushRequest>,
) -> impl IntoResponse {
    // Simplified: just log them for now, in real app we'd apply them
    for op in payload.operations {
        let log = operation_log::ActiveModel {
            entity_type: Set(op.entity_type),
            entity_id: Set(op.entity_id),
            operation: Set(op.operation),
            payload: Set(op.payload),
            created_at: Set(op.created_at),
            ..Default::default()
        };
        let _ = operation_log::Entity::insert(log).exec(&db).await;
    }
    (StatusCode::OK, Json(json!({ "message": "Operations received" }))).into_response()
}

pub async fn pull_operations(
    State(db): State<DatabaseConnection>,
) -> impl IntoResponse {
    let ops = operation_log::Entity::find().all(&db).await.unwrap_or(vec![]);
    (StatusCode::OK, Json(ops)).into_response()
}

#[derive(Deserialize)]
pub struct SearchRequest {
    query: String,
}

pub async fn search_local(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<SearchRequest>,
) -> impl IntoResponse {
    use crate::models::book;
    
    // Simple LIKE search for now
    let books = book::Entity::find()
        .filter(book::Column::Title.contains(&payload.query))
        .all(&db)
        .await
        .unwrap_or(vec![]);
        
    let book_dtos: Vec<crate::models::Book> = books.into_iter().map(crate::models::Book::from).collect();
    (StatusCode::OK, Json(book_dtos)).into_response()
}

#[derive(Deserialize)]
pub struct ProxySearchRequest {
    peer_id: i32,
    query: String,
}

pub async fn proxy_search(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<ProxySearchRequest>,
) -> impl IntoResponse {
    // 1. Find peer
    let peer = peer::Entity::find_by_id(payload.peer_id).one(&db).await.unwrap_or(None);
    
    if let Some(peer) = peer {
        // 2. Call peer's search endpoint
        let client = reqwest::Client::new();
        let url = format!("{}/api/peers/search", peer.url);
        
        let res = client.post(&url)
            .json(&json!({ "query": payload.query }))
            .send()
            .await;
            
        match res {
            Ok(response) => {
                if response.status().is_success() {
                    let books: Vec<crate::models::Book> = response.json().await.unwrap_or(vec![]);
                    return (StatusCode::OK, Json(books)).into_response();
                }
            },
            Err(_) => return (StatusCode::BAD_GATEWAY, Json(json!({ "error": "Failed to contact peer" }))).into_response(),
        }
    }
    
    (StatusCode::NOT_FOUND, Json(json!({ "error": "Peer not found" }))).into_response()
}
