//! Local, proxied and federated peer search.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use futures::future::join_all;
use sea_orm::{ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct SearchRequest {
    query: String,
}

pub async fn search_local(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<SearchRequest>,
) -> impl IntoResponse {
    use crate::models::book;
    use sea_orm::sea_query::Expr;

    let books = book::Entity::find()
        .filter(book::Column::Private.eq(false))
        .filter(
            Condition::any()
                .add(book::Column::Title.contains(&payload.query))
                .add(
                    Expr::col(book::Column::Id)
                        .in_subquery(crate::models::Book::author_search_subquery(&payload.query)),
                ),
        )
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let mut book_dtos = crate::models::Book::populate_authors(&db, books).await;
    crate::models::Book::rewrite_local_cover_urls(&mut book_dtos, None);
    (StatusCode::OK, Json(book_dtos)).into_response()
}

#[derive(Deserialize)]
pub struct ProxySearchRequest {
    peer_id: Option<i32>,
    peer_url: Option<String>,
    query: String,
    page: Option<u64>,
    limit: Option<u64>,
}

/// Plaintext HTTP proxy: fetch books from a peer URL directly.
/// When `page`/`limit` are provided, returns `{ "books": [...], "total": N, "has_more": bool }`.
/// Without pagination params, returns a flat `Vec<Book>` array (legacy).
/// The peer's response carries `added_at` directly (the owner's
/// `books.created_at`), so the "new" badge works without local enrichment.
async fn plaintext_proxy_search(
    peer_url: &str,
    query: &str,
    page: Option<u64>,
    limit: Option<u64>,
) -> axum::response::Response {
    let client = get_safe_client();
    let res = if query.is_empty() {
        let mut url = format!("{}/api/books?owned_only=true", peer_url);
        if let Some(p) = page {
            let l = limit.unwrap_or(20).min(50);
            url.push_str(&format!("&page={}&limit={}", p, l));
        }
        client.get(&url).send().await
    } else {
        let url = format!("{}/api/peers/search", peer_url);
        client
            .post(&url)
            .json(&json!({ "query": query }))
            .send()
            .await
    };

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // /api/books returns {"books": [...], "total": N}
                // /api/peers/search returns [...]
                let body: serde_json::Value = response.json().await.unwrap_or(json!([]));

                if page.is_some() && query.is_empty() {
                    // Paginated: return envelope with has_more
                    let books: Vec<crate::models::Book> = body
                        .get("books")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let total = body.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                    let p = page.unwrap_or(0);
                    let l = limit.unwrap_or(20).min(50);
                    let has_more = ((p + 1) * l) < total;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "books": books,
                            "total": total,
                            "has_more": has_more,
                        })),
                    )
                        .into_response()
                } else {
                    // Legacy: return flat array
                    let books: Vec<crate::models::Book> = if let Some(arr) = body.get("books") {
                        serde_json::from_value(arr.clone()).unwrap_or_default()
                    } else {
                        serde_json::from_value(body).unwrap_or_default()
                    };
                    (StatusCode::OK, Json(books)).into_response()
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned an error" })),
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

pub async fn proxy_search(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<ProxySearchRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find peer by id or url
    let peer = if let Some(id) = payload.peer_id {
        peer::Entity::find_by_id(id).one(db).await.unwrap_or(None)
    } else if let Some(ref url) = payload.peer_url {
        peer::Entity::find()
            .filter(peer::Column::Url.eq(url.as_str()))
            .one(db)
            .await
            .unwrap_or(None)
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer_id or peer_url required" })),
        )
            .into_response();
    };

    if let Some(peer) = peer {
        // Validate Peer URL (just in case it was modified in DB)
        if let Err(e) = validate_url(&peer.url) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }

        // Paginated library browse via E2EE (empty query + page param)
        if payload.query.is_empty() && payload.page.is_some() {
            let page = payload.page.unwrap_or(0);
            let limit = payload.limit.unwrap_or(20).min(50);
            match try_send_e2ee(
                &state,
                &peer,
                "library_browse_request",
                json!({ "page": page, "limit": limit }),
            )
            .await
            {
                Ok(Some(Some(response_msg))) => {
                    return (StatusCode::OK, Json(response_msg.payload)).into_response();
                }
                Ok(Some(None)) | Ok(None) | Err(_) => {
                    return plaintext_proxy_search(
                        &peer.url,
                        &payload.query,
                        payload.page,
                        payload.limit,
                    )
                    .await;
                }
            }
        }

        // Try E2EE path first (search is request-response: returns encrypted results)
        match try_send_e2ee(
            &state,
            &peer,
            "search_request",
            json!({ "query": payload.query }),
        )
        .await
        {
            Ok(Some(Some(response_msg))) => {
                // Got encrypted search results
                let results: Vec<crate::models::Book> = serde_json::from_value(
                    response_msg
                        .payload
                        .get("results")
                        .cloned()
                        .unwrap_or(json!([])),
                )
                .unwrap_or_default();
                return (StatusCode::OK, Json(results)).into_response();
            }
            Ok(Some(None)) => {
                // E2EE sent but no response body (unexpected for search)
                return (StatusCode::OK, Json(Vec::<crate::models::Book>::new())).into_response();
            }
            Ok(None) => {} // Fallback to plaintext
            Err(e) => {
                tracing::warn!("E2EE proxy_search failed, falling back to plaintext: {}", e);
            }
        }

        // 2. Legacy plaintext fallback
        return plaintext_proxy_search(&peer.url, &payload.query, payload.page, payload.limit)
            .await;
    }

    // Peer not in DB but URL provided (e.g. unsaved mDNS peer): direct plaintext fetch.
    // SSRF defense (ADR-026): route through ensure_registered_peer_or_mdns with
    // allow_unregistered_lan=true so the traversal is logged on the ssrf:mdns
    // tracing target. ensure_* also reconciles the trailing-slash discrepancy
    // with the peer lookup above, so a DB hit via helper still routes through
    // the enriched path instead of the unsaved branch.
    if let Some(ref url) = payload.peer_url {
        if let Err(e) = validate_url(url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }
        match ensure_registered_peer_or_mdns(db, url, true).await {
            Ok(Some(matched)) => {
                return plaintext_proxy_search(
                    &matched.url,
                    &payload.query,
                    payload.page,
                    payload.limit,
                )
                .await;
            }
            Ok(None) => {
                return plaintext_proxy_search(url, &payload.query, payload.page, payload.limit)
                    .await;
            }
            Err(status) => return status.into_response(),
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "Peer not found" })),
    )
        .into_response()
}

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
                    match res.json::<Vec<crate::models::Book>>().await {
                        Ok(mut books) => {
                            // Tag source and embed peer_id for request
                            for b in &mut books {
                                b.source = Some(format!("Peer: {}", peer.name));
                                // Hack: Embed peer_id in source_data so frontend can use it
                                b.source_data = Some(json!({ "peer_id": peer.id }).to_string());
                            }
                            books
                        }
                        _ => {
                            vec![]
                        }
                    }
                }
                Err(_) => vec![],
            }
        }
    });

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}
