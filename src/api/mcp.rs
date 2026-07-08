use crate::infrastructure::auth::LoopbackOnly;
use crate::models::{author, book, book_authors};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "mcp")]
use std::time::Duration;
#[cfg(feature = "mcp")]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Loopback ports probed to find a running BiblioGenius app, mirroring the app's
/// own bind range (`start_server` in frb.rs tries `port..port + 10`).
#[cfg(feature = "mcp")]
const MCP_PORT_SCAN_START: u16 = 8000;
#[cfg(feature = "mcp")]
const MCP_PORT_SCAN_COUNT: u16 = 10;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct JsonRpcError {
    code: i32,
    message: String,
    data: Option<Value>,
}

/// True when an `/api/health` body identifies a BiblioGenius backend (as opposed
/// to a foreign service that happens to hold the probed port, e.g. a stray Docker
/// container on 8000).
#[cfg(feature = "mcp")]
fn health_body_is_bibliogenius(body: &Value) -> bool {
    body.get("service").and_then(|v| v.as_str()) == Some("bibliogenius")
}

/// Probe loopback for a running BiblioGenius app and return its API base URL.
///
/// This is the primary data path (Option C): the running app already holds a
/// correctly-initialized database (right path, right cr-sqlite features, no
/// sandbox restriction), so the MCP helper proxies to it instead of opening the
/// database itself. Discovery is by HTTP probe rather than the app's port file,
/// because the port file lives in a Caches directory that a sandboxed helper sees
/// remapped to its own (empty) container.
#[cfg(feature = "mcp")]
async fn discover_running_app() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(400))
        .build()
        .ok()?;

    for port in MCP_PORT_SCAN_START..MCP_PORT_SCAN_START + MCP_PORT_SCAN_COUNT {
        let base = format!("http://127.0.0.1:{}", port);
        let Ok(resp) = client.get(format!("{}/api/health", base)).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        if let Ok(body) = resp.json::<Value>().await
            && health_body_is_bibliogenius(&body)
        {
            return Some(base);
        }
    }
    None
}

/// Forward a raw JSON-RPC line to the running app's internal MCP endpoint.
/// Returns `Ok(None)` for notifications (the app replies 204 No Content), or an
/// `Err` when the app is unreachable so the caller can fall back to direct access.
#[cfg(feature = "mcp")]
async fn proxy_to_app(
    client: &reqwest::Client,
    base: &str,
    raw_line: &str,
) -> Result<Option<JsonRpcResponse>, String> {
    let resp = client
        .post(format!("{}/api/mcp/rpc", base))
        .header("content-type", "application/json")
        .body(raw_line.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("upstream returned status {}", resp.status()));
    }
    resp.json::<JsonRpcResponse>()
        .await
        .map(Some)
        .map_err(|e| e.to_string())
}

#[cfg(feature = "mcp")]
fn error_response(id: Option<Value>, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        result: None,
        error: Some(JsonRpcError {
            code: -32603,
            message: message.into(),
            data: None,
        }),
        id,
    }
}

/// Internal HTTP endpoint that lets the standalone `--mcp` helper proxy JSON-RPC
/// to this running app, which already holds the correctly-initialized database.
///
/// Loopback-only on purpose: the MCP helper always runs on the same machine, and
/// the private library must never be queryable by LAN peers that can otherwise
/// reach this shared (0.0.0.0-bound) router.
pub(crate) async fn rpc_endpoint(
    _guard: LoopbackOnly,
    State(db): State<DatabaseConnection>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let is_notification = req.id.is_none();
    match handle_request(req, &db, is_notification).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Run the stdio MCP server spawned by an AI assistant (Claude Desktop, etc.).
///
/// Requests are served by whichever backend works, in priority order:
///  1. Proxy to the running BiblioGenius app over loopback HTTP (Option C). This
///     is preferred because the app already holds a correctly-initialized database
///     regardless of platform sandboxing or cr-sqlite build features.
///  2. Fall back to the locally-opened database (`db`, when available) so the tools
///     still work when the app is not running (needs a readable, feature-compatible
///     database — e.g. a non-sandboxed desktop build).
///
/// `db` is `None` when the local database could not be opened (sandbox / CRR /
/// missing features); in that case only the proxy path is available.
#[cfg(feature = "mcp")]
pub async fn start_server(db: Option<DatabaseConnection>) {
    tracing::info!("Starting Manual MCP Server (Async JSON-RPC over Stdio)...");

    let upstream = discover_running_app().await;
    match &upstream {
        Some(base) => tracing::info!("MCP: proxying requests to running app at {}", base),
        None if db.is_some() => tracing::info!("MCP: app not detected, using local database"),
        None => tracing::warn!(
            "MCP: app not detected and no local database available; tool calls will error"
        ),
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok();

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }

                // Parse request
                match serde_json::from_str::<JsonRpcRequest>(input) {
                    Ok(req) => {
                        // Notifications (no id) should not receive a response
                        let is_notification = req.id.is_none();

                        let response =
                            dispatch(req, input, is_notification, &upstream, &http, &db).await;

                        if let Some(response) = response {
                            let output = serde_json::to_string(&response).unwrap();

                            if let Err(e) =
                                stdout.write_all(format!("{}\n", output).as_bytes()).await
                            {
                                tracing::error!("Failed to write response: {}", e);
                                break;
                            }
                            if let Err(e) = stdout.flush().await {
                                tracing::error!("Failed to flush stdout: {}", e);
                                break;
                            }
                        }
                        // If None returned, it was a notification - no response needed
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse JSON-RPC: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Error reading stdin: {}", e);
                break;
            }
        }
    }
}

/// Route a single request through the proxy path first, then the local database.
#[cfg(feature = "mcp")]
async fn dispatch(
    req: JsonRpcRequest,
    raw_line: &str,
    is_notification: bool,
    upstream: &Option<String>,
    http: &Option<reqwest::Client>,
    db: &Option<DatabaseConnection>,
) -> Option<JsonRpcResponse> {
    let id = req.id.clone();

    // 1. Proxy to the running app when one was discovered.
    if let (Some(base), Some(client)) = (upstream, http) {
        match proxy_to_app(client, base, raw_line).await {
            Ok(result) => return result,
            Err(e) => tracing::warn!("MCP: proxy to app failed ({}), trying local database", e),
        }
    }

    // 2. Fall back to the locally-opened database.
    if let Some(db) = db {
        return handle_request(req, db, is_notification).await;
    }

    // 3. Neither path is available.
    if is_notification {
        None
    } else {
        Some(error_response(
            id,
            "BiblioGenius is not reachable: open the app so MCP can read your library.",
        ))
    }
}

pub(crate) async fn handle_request(
    req: JsonRpcRequest,
    db: &DatabaseConnection,
    is_notification: bool,
) -> Option<JsonRpcResponse> {
    // Handle notifications (no response expected)
    if is_notification {
        // Log the notification but don't respond
        tracing::debug!("Received notification: {}", req.method);
        return None;
    }

    let result = match req.method.as_str() {
        "initialize" => {
            // MCP Initialize response
            // Extract the client's protocol version and echo it back for compatibility
            let client_protocol_version = req
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");

            tracing::info!(
                "MCP client connected with protocol version: {}",
                client_protocol_version
            );

            serde_json::json!({
                "protocolVersion": client_protocol_version,
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "bibliogenius-mcp",
                    "version": "0.1.0"
                }
            })
        }
        "tools/list" => {
            serde_json::json!({
                "tools": [
                    {
                        "name": "search_books",
                        "description": "Search for books in the library by title or author",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "description": "Search query (matches title or author name)" }
                            },
                            "required": ["query"]
                        }
                    },
                    {
                        "name": "get_statistics",
                        "description": "Get library statistics (count of books, readiness status)",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    }
                ]
            })
        }
        "tools/call" => {
            if let Some(params) = req.params {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));

                match name {
                    "search_books" => {
                        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");

                        // 1. Search by title
                        let books_by_title = book::Entity::find()
                            .filter(book::Column::Title.contains(query))
                            .limit(10)
                            .all(db)
                            .await
                            .unwrap_or_default();

                        // 2. Search by author name - find authors matching query
                        let matching_authors = author::Entity::find()
                            .filter(author::Column::Name.contains(query))
                            .all(db)
                            .await
                            .unwrap_or_default();

                        // 3. Find books by those authors
                        let mut books_by_author: Vec<book::Model> = Vec::new();
                        for auth in &matching_authors {
                            let author_books = book_authors::Entity::find()
                                .filter(book_authors::Column::AuthorId.eq(auth.id.as_str()))
                                .all(db)
                                .await
                                .unwrap_or_default();

                            for ba in author_books {
                                if let Ok(Some(b)) =
                                    book::Entity::find_by_id(ba.book_id).one(db).await
                                {
                                    books_by_author.push(b);
                                }
                            }
                        }

                        // 4. Combine and dedupe results
                        let mut all_books = books_by_title;
                        for b in books_by_author {
                            if !all_books.iter().any(|existing| existing.id == b.id) {
                                all_books.push(b);
                            }
                        }

                        // Limit to 10 results
                        all_books.truncate(10);

                        // 5. Format output with author names
                        let mut book_list: Vec<String> = Vec::new();
                        for b in all_books {
                            // Fetch authors for this book
                            let book_author_links = book_authors::Entity::find()
                                .filter(book_authors::Column::BookId.eq(b.id))
                                .all(db)
                                .await
                                .unwrap_or_default();

                            let mut author_names: Vec<String> = Vec::new();
                            for ba in book_author_links {
                                if let Ok(Some(auth)) =
                                    author::Entity::find_by_id(ba.author_id).one(db).await
                                {
                                    author_names.push(auth.name);
                                }
                            }

                            let author_str = if author_names.is_empty() {
                                "Unknown author".to_string()
                            } else {
                                author_names.join(", ")
                            };

                            book_list.push(format!(
                                "- {} by {} ({}): {}",
                                b.title,
                                author_str,
                                b.publication_year.unwrap_or(0),
                                b.summary.clone().unwrap_or("No summary".to_string())
                            ));
                        }

                        if book_list.is_empty() {
                            serde_json::json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("No books found matching '{}' in title or author", query)
                                    }
                                ]
                            })
                        } else {
                            serde_json::json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("Found {} books matching '{}':\n{}", book_list.len(), query, book_list.join("\n"))
                                    }
                                ]
                            })
                        }
                    }
                    "get_statistics" => {
                        let count = book::Entity::find().count(db).await.unwrap_or(0);
                        let to_read = book::Entity::find()
                            .filter(book::Column::ReadingStatus.eq("to_read"))
                            .count(db)
                            .await
                            .unwrap_or(0);
                        let reading = book::Entity::find()
                            .filter(book::Column::ReadingStatus.eq("reading"))
                            .count(db)
                            .await
                            .unwrap_or(0);

                        serde_json::json!({
                            "content": [
                                {
                                    "type": "text",
                                    "text": format!("Library Statistics:\n- Total Books: {}\n- To Read: {}\n- Currently Reading: {}", count, to_read, reading)
                                }
                            ]
                        })
                    }
                    _ => {
                        return Some(JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: None,
                            error: Some(JsonRpcError {
                                code: -32601,
                                message: format!("Method not found: {}", name),
                                data: None,
                            }),
                            id: req.id,
                        });
                    }
                }
            } else {
                serde_json::json!({})
            }
        }
        _ => {
            // Unknown method - return error
            return Some(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: "Method not found".to_string(),
                    data: None,
                }),
                id: req.id,
            });
        }
    };

    Some(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        result: Some(result),
        error: None,
        id: req.id,
    })
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn health_identity_accepts_bibliogenius_only() {
        assert!(health_body_is_bibliogenius(&json!({
            "status": "ok",
            "service": "bibliogenius",
            "version": "1.1.1"
        })));
        // A foreign service squatting the probed port must not be mistaken for the app.
        assert!(!health_body_is_bibliogenius(&json!({ "status": "ok" })));
        assert!(!health_body_is_bibliogenius(
            &json!({ "service": "grafana" })
        ));
        assert!(!health_body_is_bibliogenius(&json!("ok")));
    }

    #[test]
    fn port_scan_range_matches_apps_bind_window() {
        // The helper must probe the same window the app can bind to, otherwise a
        // port-drifted app (8000 taken) would be missed.
        let last = MCP_PORT_SCAN_START + MCP_PORT_SCAN_COUNT - 1;
        assert_eq!(MCP_PORT_SCAN_START, 8000);
        assert_eq!(last, 8009);
    }

    #[test]
    fn error_response_is_shaped_for_a_request_id() {
        let resp = error_response(Some(json!(7)), "boom");
        assert_eq!(resp.id, Some(json!(7)));
        assert!(resp.result.is_none());
        let err = resp.error.expect("error present");
        assert_eq!(err.message, "boom");
    }
}
