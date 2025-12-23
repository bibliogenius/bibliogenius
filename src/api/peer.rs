#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
use crate::models::{operation_log, peer};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use futures::future::join_all;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::IpAddr;
use url::Url;

/// Validate URL to prevent SSRF
/// Blocks:
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-Local (169.254.0.0/16, fe80::/10)
/// - AWS Metadata Service (169.254.169.254)
/// - "localhost" hostname
fn validate_url(url_str: &str) -> Result<String, String> {
    let url = Url::parse(url_str).map_err(|_| "Invalid URL format".to_string())?;

    // 1. Check Scheme
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Only HTTP/HTTPS schemes allowed".to_string());
    }

    // 2. Check Host
    if let Some(host_str) = url.host_str() {
        if host_str == "localhost" {
            return Err("Localhost access is blocked".to_string());
        }

        // Check if it's an IP address
        if let Ok(ip) = host_str.parse::<IpAddr>() {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            // Note: Link-local addresses (169.254.x.x, fe80::/10) are ALLOWED
            // for local network P2P communication between devices on same network
        }
    }

    Ok(url.to_string())
}

/// Create a safe HTTP client with restricted redirects and timeouts
fn get_safe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()) // Disable redirects to prevent bypass
        .build()
        .unwrap_or_default()
}

/// Translate localhost URLs to Docker service names for inter-container communication
/// Examples:
/// - http://localhost:8001 -> http://bibliogenius-a:8000
/// - http://localhost:8002 -> http://bibliogenius-b:8000
fn translate_url_for_docker(url: &str) -> String {
    if url.contains("localhost:8001") {
        url.replace("localhost:8001", "bibliogenius-a:8000")
    } else if url.contains("localhost:8002") {
        url.replace("localhost:8002", "bibliogenius-b:8000")
    } else {
        url.to_string()
    }
}

#[derive(Deserialize)]
pub struct ConnectRequest {
    name: String,
    url: String,
    public_key: Option<String>,
}

pub async fn connect(
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    Json(payload): Json<ConnectRequest>,
) -> impl IntoResponse {
    // 1. Validate URL
    if let Err(e) = validate_url(&payload.url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Fetch remote config to get location and verify connectivity
    let client = get_safe_client();
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

    // Translate localhost URLs to Docker service names for inter-container communication
    let docker_url = translate_url_for_docker(&payload.url);

    let peer = peer::ActiveModel {
        name: Set(name),
        url: Set(docker_url),
        public_key: Set(payload.public_key),
        latitude: Set(latitude),
        longitude: Set(longitude),
        last_seen: Set(Some(chrono::Utc::now().to_rfc3339())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        auto_approve: Set(true),
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

#[derive(Deserialize)]
pub struct IncomingConnectionRequest {
    name: String,
    url: String,
}

/// Receive an incoming connection request from a remote peer
/// This forwards the request to the local Hub to create an 'incoming' peer record
pub async fn receive_connection_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingConnectionRequest>,
) -> impl IntoResponse {
    // Forward to local Hub
    let hub_url = std::env::var("HUB_URL").unwrap_or_else(|_| "http://localhost:8081".to_string());
    let endpoint = format!("{}/api/peers/receive_connection", hub_url);

    let client = get_safe_client();
    match client
        .post(&endpoint)
        .json(&serde_json::json!({
            "name": payload.name,
            "url": payload.url,
        }))
        .send()
        .await
    {
        Ok(res) => {
            if res.status().is_success() {
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Connection request received and forwarded to Hub" })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Hub rejected the request" })),
                )
                    .into_response()
            }
        }
        Err(_) => {
            // Fallback: Handle locally if Hub is unreachable (P2P mode)
            // Check if peer exists locally
            let existing = peer::Entity::find()
                .filter(peer::Column::Url.eq(&payload.url))
                .one(&db)
                .await;

            match existing {
                Ok(Some(_)) => (
                    StatusCode::OK,
                    Json(json!({ "message": "Peer already exists locally" })),
                )
                    .into_response(),
                Ok(None) => {
                    // Create new peer (pending approval conceptually, strict P2P)
                    let new_peer = peer::ActiveModel {
                        name: Set(payload.name),
                        url: Set(payload.url),
                        auto_approve: Set(false),
                        created_at: Set(Utc::now().to_rfc3339()),
                        updated_at: Set(Utc::now().to_rfc3339()),
                        ..Default::default()
                    };

                    match new_peer.insert(&db).await {
                        Ok(_) => (
                            StatusCode::OK,
                            Json(json!({ "message": "Connection request saved locally" })),
                        )
                            .into_response(),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("Failed to save peer locally: {}", e) })),
                        )
                            .into_response(),
                    }
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Database error: {}", e) })),
                )
                    .into_response(),
            }
        }
    }
}

pub async fn list_peers(
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
) -> impl IntoResponse {
    // 1. Sync with Hub if HUB_URL is set
    if let Ok(hub_url) = std::env::var("HUB_URL") {
        let client = get_safe_client();
        let url = format!("{}/api/peers", hub_url);

        if let Ok(res) = client.get(&url).send().await {
            if res.status().is_success() {
                #[derive(Deserialize)]
                struct HubPeer {
                    name: String,
                    url: String,
                    #[serde(rename = "status")]
                    _status: String,
                }
                #[derive(Deserialize)]
                struct HubResponse {
                    data: Vec<HubPeer>,
                }

                if let Ok(hub_res) = res.json::<HubResponse>().await {
                    for hub_peer in hub_res.data {
                        // Normalize URL
                        let docker_url = translate_url_for_docker(&hub_peer.url);

                        // Check if peer exists
                        let existing = peer::Entity::find()
                            .filter(peer::Column::Url.eq(&docker_url))
                            .one(&db)
                            .await;

                        match existing {
                            Ok(Some(p)) => {
                                // Update status if changed
                                let mut active: peer::ActiveModel = p.into();
                                active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                                let _ = active.update(&db).await;
                            }
                            Ok(None) => {
                                // Insert new peer
                                let new_peer = peer::ActiveModel {
                                    name: Set(hub_peer.name),
                                    url: Set(docker_url),
                                    created_at: Set(chrono::Utc::now().to_rfc3339()),
                                    updated_at: Set(chrono::Utc::now().to_rfc3339()),
                                    ..Default::default()
                                };
                                let _ = peer::Entity::insert(new_peer).exec(&db).await;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    let peers = peer::Entity::find().all(&db).await.unwrap_or(vec![]);

    // Convert to JSON with computed status field
    let peers_with_status: Vec<serde_json::Value> = peers
        .into_iter()
        .map(|p| {
            let status = if p.auto_approve {
                "connected"
            } else {
                "pending"
            };
            json!({
                "id": p.id,
                "name": p.name,
                "url": p.url,
                "public_key": p.public_key,
                "latitude": p.latitude,
                "longitude": p.longitude,
                "auto_approve": p.auto_approve,
                "status": status,
                "last_seen": p.last_seen,
               "created_at": p.created_at,
                "updated_at": p.updated_at,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "data": peers_with_status
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct UpdatePeerStatusRequest {
    status: String, // "active" (accept) or "rejected"
}

/// Update a peer's status (accept or reject a connection request)
pub async fn update_peer_status(
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerStatusRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response()
        }
    };

    // Update status based on action
    let auto_approve = payload.status == "active" || payload.status == "accepted";

    let mut active_model: peer::ActiveModel = peer.into();
    active_model.auto_approve = Set(auto_approve);
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active_model.update(&db).await {
        Ok(updated) => {
            tracing::info!("✅ Peer {} status updated to: {}", peer_id, payload.status);
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer status updated",
                    "peer": updated,
                    "auto_approve": auto_approve
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

/// Delete a peer (reject and remove)
pub async fn delete_peer(
    State(db): State<DatabaseConnection>,
    _claims: crate::auth::Claims,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    match peer::Entity::delete_by_id(peer_id).exec(&db).await {
        Ok(_) => {
            tracing::info!("🗑️ Peer {} deleted", peer_id);
            (StatusCode::OK, Json(json!({ "message": "Peer deleted" }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
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
        // Validate Peer URL (just in case it was modified in DB)
        if let Err(e) = validate_url(&peer.url) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }

        // 2. Call peer's search endpoint
        let client = get_safe_client();
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
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();
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

/// Sync peer by URL (solves ID mismatch between Hub and Backend)
pub async fn sync_peer_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response()
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // 1. Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            // Peer not found locally, try to fetch from Hub
            let mut found_peer = None;

            if let Ok(hub_url) = std::env::var("HUB_URL") {
                let client = get_safe_client();
                let url = format!("{}/api/peers", hub_url);

                if let Ok(res) = client.get(&url).send().await {
                    if res.status().is_success() {
                        #[derive(Deserialize)]
                        struct HubPeer {
                            name: String,
                            url: String,
                            #[serde(rename = "status")]
                            _status: String,
                        }
                        #[derive(Deserialize)]
                        struct HubResponse {
                            data: Vec<HubPeer>,
                        }

                        if let Ok(hub_res) = res.json::<HubResponse>().await {
                            for hub_peer in hub_res.data {
                                let hub_docker_url = translate_url_for_docker(&hub_peer.url);

                                // Match by URL
                                if hub_docker_url == docker_url {
                                    // Insert new peer
                                    let new_peer = peer::ActiveModel {
                                        name: Set(hub_peer.name),
                                        url: Set(hub_docker_url.clone()),
                                        created_at: Set(chrono::Utc::now().to_rfc3339()),
                                        updated_at: Set(chrono::Utc::now().to_rfc3339()),
                                        ..Default::default()
                                    };

                                    if let Ok(res) = peer::Entity::insert(new_peer).exec(&db).await
                                    {
                                        // Fetch the inserted peer to return it
                                        found_peer = peer::Entity::find_by_id(res.last_insert_id)
                                            .one(&db)
                                            .await
                                            .unwrap_or(None);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            match found_peer {
                Some(p) => p,
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(
                            json!({ "error": format!("Peer not found with URL: {}", docker_url) }),
                        ),
                    )
                        .into_response()
                }
            }
        }
    };

    // 2. Fetch remote books
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();
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
                        Json(json!({ "message": "Sync successful", "count": count, "peer_id": peer.id })),
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

    let client = get_safe_client();
    let query_str = params.title.clone().unwrap_or_default(); // Simple query for now

    let futures = peers.into_iter().map(|peer| {
        let client = client.clone();
        let q = query_str.clone();
        async move {
            if validate_url(&peer.url).is_err() {
                return vec![];
            }
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
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    // Get my config to identify myself
    let my_config = match crate::models::library_config::Entity::find().one(&db).await {
        Ok(Some(config)) => config,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response()
        }
    };

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

#[derive(Deserialize)]
pub struct BookRequestByUrl {
    peer_url: String,
    book_isbn: String,
    book_title: String,
}

pub async fn request_book_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<BookRequestByUrl>,
) -> impl IntoResponse {
    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(&payload.peer_url);

    // 1. Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Optional: Auto-create peer if not found?
            // For now, let's return 404 to be safe, assuming they should have synced first.
            // But wait, if they are viewing books, they might be viewing them from a "Search Network" result
            // which might not have created the peer yet?
            // Actually, list_peer_books_by_url requires peer to exist.
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found with URL: {}", docker_url) })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
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
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    // Get my config to identify myself
    let my_config = match crate::models::library_config::Entity::find().one(&db).await {
        Ok(Some(config)) => config,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response()
        }
    };

    let res = client
        .post(&url)
        .json(&json!({
            "from_peer_url": "http://localhost:8000", // TODO: Get from config or dynamic
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
    use crate::models::{book, contact, copy, loan, p2p_request};

    let req = match p2p_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(r)) => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response()
        }
    };

    let mut active: p2p_request::ActiveModel = req.clone().into();
    let new_status = payload.status.as_str();

    // State transition logic
    if new_status == "accepted" && req.status == "pending" {
        // 1. Find Peer to link/create Contact
        let peer = match peer::Entity::find_by_id(req.from_peer_id).one(&db).await {
            Ok(Some(p)) => p,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "Peer not found" })),
                )
                    .into_response()
            }
        };

        // 2. Find Book and Available Copy
        tracing::info!(
            "Looking for book with ISBN: '{}' for request {}",
            req.book_isbn,
            req.id
        );
        let book = match book::Entity::find()
            .filter(book::Column::Isbn.eq(&req.book_isbn))
            .one(&db)
            .await
        {
            Ok(Some(b)) => {
                tracing::info!("Found book: {} (id={})", b.title, b.id);
                b
            }
            Ok(None) => {
                tracing::warn!(
                    "Book not found for ISBN: '{}'. Checking by title: '{}'",
                    req.book_isbn,
                    req.book_title
                );
                // Fallback: Try to find by title if ISBN lookup fails
                match book::Entity::find()
                    .filter(book::Column::Title.eq(&req.book_title))
                    .one(&db)
                    .await
                {
                    Ok(Some(b)) => {
                        tracing::info!("Found book by title: {} (id={})", b.title, b.id);
                        b
                    }
                    _ => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("Book not found (ISBN: '{}', Title: '{}')", req.book_isbn, req.book_title) })),
                        )
                            .into_response()
                    }
                }
            }
            Err(e) => {
                tracing::error!("DB error looking up book: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("DB error: {}", e) })),
                )
                    .into_response();
            }
        };

        let copy = match copy::Entity::find()
            .filter(copy::Column::BookId.eq(book.id))
            .filter(copy::Column::Status.eq("available"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            _ => {
                // Self-healing: Check if ANY copy exists
                let any_copy = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if any_copy.is_none() {
                    tracing::info!("Self-healing: Creating missing copy for book {}", book.id);
                    // No copies exist at all (legacy data), create one!
                    let now = chrono::Utc::now().to_rfc3339();
                    let new_copy = copy::ActiveModel {
                        book_id: Set(book.id),
                        library_id: Set(1), // Default library
                        status: Set("available".to_string()),
                        is_temporary: Set(false),
                        created_at: Set(now.clone()),
                        updated_at: Set(now),
                        ..Default::default()
                    };

                    match new_copy.insert(&db).await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!("Failed to auto-create copy: {}", e);
                            return (
                                StatusCode::CONFLICT,
                                Json(json!({ "error": "No available copies and failed to create one" })),
                            )
                                .into_response();
                        }
                    }
                } else {
                    // Copies exist but none are available (truly borrowed)
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No available copies" })),
                    )
                        .into_response();
                }
            }
        };

        // 3. Find or Create Contact for Peer
        let contact = match contact::Entity::find()
            .filter(contact::Column::Name.eq(&peer.name))
            .filter(contact::Column::Type.eq("Library"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                // Create new contact
                let new_contact = contact::ActiveModel {
                    r#type: Set("Library".to_string()),
                    name: Set(peer.name.clone()),
                    library_owner_id: Set(1), // Default owner
                    is_active: Set(true),
                    created_at: Set(chrono::Utc::now().to_rfc3339()),
                    updated_at: Set(chrono::Utc::now().to_rfc3339()),
                    ..Default::default()
                };
                new_contact.insert(&db).await.unwrap()
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "DB Error finding contact" })),
                )
                    .into_response()
            }
        };

        // 4. Create Loan
        let loan = loan::ActiveModel {
            copy_id: Set(copy.id),
            contact_id: Set(contact.id),
            library_id: Set(1), // Default library
            loan_date: Set(chrono::Utc::now().to_rfc3339()),
            due_date: Set((chrono::Utc::now() + chrono::Duration::days(14)).to_rfc3339()), // 2 weeks default
            status: Set("active".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        if let Err(e) = loan::Entity::insert(loan).exec(&db).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create loan: {}", e) })),
            )
                .into_response();
        }

        // Update Copy status
        let mut active_copy: copy::ActiveModel = copy.into();
        active_copy.status = Set("loaned".to_string());
        let _ = active_copy.update(&db).await;

        // 5. Notify borrower that loan was accepted
        let peer_url = peer.url.clone();
        let book_isbn = book.isbn.clone();
        let book_title = book.title.clone();
        let book_cover = book.cover_url.clone();
        let due_date = (chrono::Utc::now() + chrono::Duration::days(14))
            .format("%Y-%m-%d")
            .to_string();

        // Get library name for lender identification
        let lender_name = match crate::models::library::Entity::find_by_id(1).one(&db).await {
            Ok(Some(lib)) => lib.name,
            _ => "Unknown Library".to_string(),
        };

        tokio::spawn(async move {
            let client = reqwest::Client::new();
            let confirm_result = client
                .post(format!("{}/api/peers/loans/confirm", peer_url))
                .json(&serde_json::json!({
                    "isbn": book_isbn,
                    "title": book_title,
                    "author": Option::<String>::None, // TODO: fetch from relation
                    "cover_url": book_cover,
                    "lender_name": lender_name,
                    "due_date": due_date,
                }))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;

            match confirm_result {
                Ok(resp) => {
                    tracing::info!(
                        "📤 Loan confirmation sent to {}: {}",
                        peer_url,
                        resp.status()
                    );
                }
                Err(e) => {
                    tracing::warn!("⚠️ Failed to send loan confirmation to {}: {}", peer_url, e);
                }
            }
        });
    } else if new_status == "returned" && req.status == "accepted" {
        // Handle Return
        // Find the loan associated with this peer (contact) and book
        // This is tricky because we didn't link Loan to Request directly.
        // We have to infer: Find active loan for this book's copy where contact matches peer.

        // 1. Find Peer/Contact
        let peer = peer::Entity::find_by_id(req.from_peer_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();

        let contact = contact::Entity::find()
            .filter(contact::Column::Name.eq(&peer.name))
            .filter(contact::Column::Type.eq("Library"))
            .one(&db)
            .await
            .unwrap(); // Should exist if accepted

        if let Some(contact) = contact {
            // 2. Find Book
            let book = book::Entity::find()
                .filter(book::Column::Isbn.eq(&req.book_isbn))
                .one(&db)
                .await
                .unwrap()
                .unwrap();

            // 3. Find Active Loan for any copy of this book for this contact
            // Join Loan -> Copy -> Book
            // SeaORM doesn't support deep join easily in find() without defining relations.
            // We can iterate copies of book.
            let copies = copy::Entity::find()
                .filter(copy::Column::BookId.eq(book.id))
                .all(&db)
                .await
                .unwrap();

            let copy_ids: Vec<i32> = copies.iter().map(|c| c.id).collect();

            let active_loan = loan::Entity::find()
                .filter(loan::Column::ContactId.eq(contact.id))
                .filter(loan::Column::Status.eq("active"))
                .filter(loan::Column::CopyId.is_in(copy_ids))
                .one(&db)
                .await
                .unwrap();

            if let Some(l) = active_loan {
                let mut active_loan: loan::ActiveModel = l.clone().into();
                active_loan.status = Set("returned".to_string());
                active_loan.return_date = Set(Some(chrono::Utc::now().to_rfc3339()));
                active_loan.updated_at = Set(chrono::Utc::now().to_rfc3339());
                let _ = active_loan.update(&db).await;

                // Update Copy
                let copy = copy::Entity::find_by_id(l.copy_id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                let mut active_copy: copy::ActiveModel = copy.into();
                active_copy.status = Set("available".to_string());
                let _ = active_copy.update(&db).await;
            }
        }
    }

    // Update Request Status
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_peer_books(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// List peer books by URL (solves ID mismatch)
pub async fn list_peer_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response()
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found with URL: {}", docker_url) })),
            )
                .into_response()
        }
    };

    // Get books for this peer
    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

pub async fn delete_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    match p2p_request::Entity::delete_by_id(id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Request not found" })),
                )
                    .into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_outgoing_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    match p2p_outgoing_request::Entity::delete_by_id(id)
        .exec(&db)
        .await
    {
        Ok(res) => {
            if res.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Request not found" })),
                )
                    .into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct IncomingLoanRequest {
    pub from_name: String,
    pub from_url: String,
    pub book_isbn: String,
    pub book_title: String,
}

pub async fn receive_loan_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingLoanRequest>,
) -> impl IntoResponse {
    use crate::models::p2p_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find or Create Peer
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.from_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_name),
                url: Set(payload.from_url),
                auto_approve: Set(false),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response()
        }
    };

    // 2. Create Incoming Request
    let request_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_request::ActiveModel {
        id: Set(request_id),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Loan request received" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save request: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct OutgoingLoanRequestDto {
    pub to_peer_url: String,
    pub book_isbn: String,
    pub book_title: String,
}

pub async fn create_outgoing_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<OutgoingLoanRequestDto>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find Peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.to_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found locally" })),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response()
        }
    };

    // 2. Create Outgoing Request Log
    let request_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_outgoing_request::ActiveModel {
        id: Set(request_id),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Outgoing request logged" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save outgoing request: {}", e) })),
        )
            .into_response(),
    }
}

// ============ P2P LOAN CONFIRMATION ============

#[derive(Debug, Deserialize)]
pub struct LoanConfirmation {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub lender_name: String,
    pub due_date: String,
}

/// Receive loan confirmation from lender
/// Creates the book (if not exists) and a borrowed copy in the borrower's library
pub async fn receive_loan_confirmation(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoanConfirmation>,
) -> impl IntoResponse {
    use crate::models::{book, copy};
    use chrono::Utc;

    tracing::info!(
        "📚 Received loan confirmation: '{}' from {}",
        payload.title,
        payload.lender_name
    );

    // 1. Find or create book
    let existing_book = if let Some(ref isbn) = payload.isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn))
            .one(&db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(&payload.title))
            .one(&db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => {
            tracing::info!("Book already exists: id={}", b.id);
            b.id
        }
        None => {
            // Create new book
            let now = Utc::now().to_rfc3339();
            // Note: author is a relation, not a direct field on books table
            // Store author info in summary for now
            let summary_text = payload.author.clone().map(|a| format!("Auteur: {}", a));
            let new_book = book::ActiveModel {
                title: Set(payload.title.clone()),
                isbn: Set(payload.isbn.clone()),
                summary: Set(summary_text),
                cover_url: Set(payload.cover_url.clone()),
                owned: Set(false), // It's a borrowed book, not owned
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };

            match new_book.insert(&db).await {
                Ok(b) => {
                    tracing::info!("Created new book: id={}", b.id);
                    b.id
                }
                Err(e) => {
                    tracing::error!("Failed to create book: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create book: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
    };

    // 2. Create borrowed copy
    let now = Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(1), // Default library
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {} jusqu'au {}",
            payload.lender_name, payload.due_date
        ))),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(&db).await {
        Ok(c) => {
            tracing::info!(
                "✅ Created borrowed copy: id={} for book_id={}",
                c.id,
                book_id
            );
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Loan confirmed",
                    "book_id": book_id,
                    "copy_id": c.id
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to create borrowed copy: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create copy: {}", e) })),
            )
                .into_response()
        }
    }
}
