use crate::models::{operation_log, peer};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use futures::future::join_all;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
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
    // 1. Fetch remote config to get location and verify connectivity
    let client = reqwest::Client::new();
    let config_url = format!("{}/api/config", payload.url.trim_end_matches('/'));

    let (latitude, longitude, remote_name) = match client.get(&config_url).send().await {
        Ok(res) => {
            if res.status().is_success() {
                if let Ok(config) = res.json::<crate::api::setup::ConfigResponse>().await {
                    let (lat, long) = if config.share_location {
                        (config.latitude, config.longitude)
                    } else {
                        (None, None)
                    };
                    (lat, long, Some(config.library_name))
                } else {
                    (None, None, None)
                }
            } else {
                (None, None, None)
            }
        }
        Err(_) => (None, None, None),
    };

    // Use provided name or fallback to remote name or "Unknown"
    let name = if !payload.name.is_empty() {
        payload.name
    } else {
        remote_name.unwrap_or_else(|| "Unknown Library".to_string())
    };

    let peer = peer::ActiveModel {
        name: Set(name),
        url: Set(payload.url),
        public_key: Set(payload.public_key),
        latitude: Set(latitude),
        longitude: Set(longitude),
        last_seen: Set(Some(chrono::Utc::now().to_rfc3339())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match peer::Entity::insert(peer).exec(&db).await {
        Ok(res) => (
            StatusCode::CREATED,
            Json(json!({ "id": res.last_insert_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let peers = peer::Entity::find().all(&db).await.unwrap_or(vec![]);
    (StatusCode::OK, Json(peers)).into_response()
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
    (
        StatusCode::OK,
        Json(json!({ "message": "Operations received" })),
    )
        .into_response()
}

pub async fn pull_operations(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let ops = operation_log::Entity::find()
        .all(&db)
        .await
        .unwrap_or(vec![]);
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

    let book_dtos: Vec<crate::models::Book> =
        books.into_iter().map(crate::models::Book::from).collect();
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
    let peer = peer::Entity::find_by_id(payload.peer_id)
        .one(&db)
        .await
        .unwrap_or(None);

    if let Some(peer) = peer {
        // 2. Call peer's search endpoint
        let client = reqwest::Client::new();
        let url = format!("{}/api/peers/search", peer.url);

        let res = client
            .post(&url)
            .json(&json!({ "query": payload.query }))
            .send()
            .await;

        match res {
            Ok(response) => {
                if response.status().is_success() {
                    let books: Vec<crate::models::Book> = response.json().await.unwrap_or(vec![]);
                    return (StatusCode::OK, Json(books)).into_response();
                }
            }
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Failed to contact peer" })),
                )
                    .into_response()
            }
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "Peer not found" })),
    )
        .into_response()
}

pub async fn sync_peer(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response()
        }
    };

    // 2. Fetch remote books
    let client = reqwest::Client::new();
    let url = format!("{}/api/books", peer.url);

    let res = client.get(&url).send().await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // Parse response: { "books": [...] }
                #[derive(Deserialize)]
                struct BooksResponse {
                    books: Vec<crate::models::Book>,
                }

                if let Ok(data) = response.json::<BooksResponse>().await {
                    // 3. Clear old cache for this peer
                    let _ = peer_book::Entity::delete_many()
                        .filter(peer_book::Column::PeerId.eq(peer.id))
                        .exec(&db)
                        .await;

                    let count = data.books.len();

                    // 4. Insert new cache
                    for book in data.books {
                        let cache = peer_book::ActiveModel {
                            peer_id: Set(peer.id),
                            remote_book_id: Set(book.id.unwrap_or(0)),
                            title: Set(book.title),
                            isbn: Set(book.isbn),
                            author: Set(book.author),
                            cover_url: Set(book.cover_url),
                            summary: Set(book.summary),
                            synced_at: Set(chrono::Utc::now().to_rfc3339()),
                            ..Default::default()
                        };
                        let _ = peer_book::Entity::insert(cache).exec(&db).await;
                    }

                    (
                        StatusCode::OK,
                        Json(json!({ "message": "Sync successful", "count": count })),
                    )
                        .into_response()
                } else {
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "Invalid response format" })),
                    )
                        .into_response()
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned error" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

// --- Federated Search Helper ---

pub async fn broadcast_search(
    db: &DatabaseConnection,
    params: &crate::api::search::SearchQuery,
) -> Vec<crate::models::Book> {
    let peers = peer::Entity::find().all(db).await.unwrap_or(vec![]);
    if peers.is_empty() {
        return vec![];
    }

    let client = reqwest::Client::new();
    let query_str = params.title.clone().unwrap_or_default(); // Simple query for now

    let futures = peers.into_iter().map(|peer| {
        let client = client.clone();
        let q = query_str.clone();
        async move {
            let url = format!("{}/api/peers/search", peer.url);
            match client
                .post(&url)
                .json(&json!({ "query": q }))
                .timeout(std::time::Duration::from_secs(2)) // 2s timeout
                .send()
                .await
            {
                Ok(res) => {
                    if let Ok(mut books) = res.json::<Vec<crate::models::Book>>().await {
                        // Tag source and embed peer_id for request
                        for b in &mut books {
                            b.source = Some(format!("Peer: {}", peer.name));
                            // Hack: Embed peer_id in source_data so frontend can use it
                            b.source_data = Some(json!({ "peer_id": peer.id }).to_string());
                        }
                        books
                    } else {
                        vec![]
                    }
                }
                Err(_) => vec![],
            }
        }
    });

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}

#[derive(Deserialize)]
pub struct BookRequest {
    book_isbn: String,
    book_title: String,
}

pub async fn request_book(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<BookRequest>,
) -> impl IntoResponse {
    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response()
        }
    };

    // 2. Save Outgoing Request
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(&db)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 3. Send request to peer
    let client = reqwest::Client::new();
    let url = format!("{}/api/peers/request", peer.url);

    // Get my config to identify myself
    let my_config = crate::models::library_config::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();

    let res = client
        .post(&url)
        .json(&json!({
            "from_peer_url": "http://localhost:8000", // TODO: Get from config
            "from_peer_name": my_config.name,
            "book_isbn": payload.book_isbn,
            "book_title": payload.book_title
        }))
        .send()
        .await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response()
            } else {
                // TODO: Update outgoing request status to 'failed' if rejected immediately?
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

pub async fn list_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let requests = crate::models::p2p_outgoing_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "status": req.status,
                "created_at": req.created_at,
                "peer_name": peer.map(|p| p.name).unwrap_or("Unknown".to_string())
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

#[derive(Deserialize)]
pub struct IncomingRequest {
    from_peer_url: String,
    from_peer_name: String,
    book_isbn: String,
    book_title: String,
}

pub async fn receive_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingRequest>,
) -> impl IntoResponse {
    // 1. Find or Create Peer
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.from_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_peer_name),
                url: Set(payload.from_peer_url),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            };
            new_peer.insert(&db).await.unwrap()
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response()
        }
    };

    // 2. Create Request Record
    let initial_status = if peer.auto_approve {
        "accepted"
    } else {
        "pending"
    };

    let request = crate::models::p2p_request::ActiveModel {
        id: Set(uuid::Uuid::new_v4().to_string()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set(initial_status.to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    match crate::models::p2p_request::Entity::insert(request)
        .exec(&db)
        .await
    {
        Ok(_) => (
            StatusCode::CREATED,
            Json(json!({ "success": true, "status": initial_status })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let requests = crate::models::p2p_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "status": req.status,
                "created_at": req.created_at,
                "peer_name": peer.map(|p| p.name).unwrap_or("Unknown".to_string())
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

#[derive(Deserialize)]
pub struct RequestAction {
    status: String,
}

pub async fn update_request_status(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
    Json(payload): Json<RequestAction>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    let req = match p2p_request::Entity::find_by_id(id).one(&db).await {
        Ok(Some(r)) => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response()
        }
    };

    let mut active: p2p_request::ActiveModel = req.into();
    active.status = Set(payload.status);
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(_) => (StatusCode::OK, Json(json!({ "success": true }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
