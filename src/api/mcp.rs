use crate::infrastructure::auth::McpAuth;
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
pub struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct JsonRpcResponse {
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
/// `Err` when the app is unreachable or rejects the token.
#[cfg(feature = "mcp")]
async fn proxy_to_app(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    raw_line: &str,
) -> Result<Option<JsonRpcResponse>, String> {
    let resp = client
        .post(format!("{}/api/mcp/rpc", base))
        .header("content-type", "application/json")
        .bearer_auth(token)
        .body(raw_line.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(
            "the app rejected the MCP token: re-copy the configuration from BiblioGenius settings"
                .to_string(),
        );
    }

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

fn success_response(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        result: Some(result),
        error: None,
        id,
    }
}

/// The `initialize` result. Echoes the client's protocol version for compatibility.
fn initialize_result(params: Option<&Value>) -> Value {
    let client_protocol_version = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or("2024-11-05");

    tracing::info!(
        "MCP client connected with protocol version: {}",
        client_protocol_version
    );

    serde_json::json!({
        "protocolVersion": client_protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "bibliogenius-mcp",
            "version": "0.1.0"
        }
    })
}

/// The `tools/list` result: the tool vocabulary exposed to AI assistants.
fn tools_list_result() -> Value {
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

/// Internal HTTP endpoint that lets the standalone `--mcp` helper proxy JSON-RPC
/// to this running app, which already holds the correctly-initialized database.
///
/// Guarded by [`McpAuth`]: loopback source, no browser `Origin`, and a valid token.
/// The private library must never be queryable by LAN peers that reach this shared
/// (0.0.0.0-bound) router, nor by a web page running in the user's own browser.
pub async fn rpc_endpoint(
    _guard: McpAuth,
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
/// The helper is a pure transport shim: it NEVER opens the database. Every request
/// that needs data is proxied to the running app over loopback HTTP, because the app
/// already holds a correctly-initialized database (right path, right cr-sqlite
/// features, no sandbox restriction). A second process on the same SQLite file was a
/// standing source of corruption-adjacent bugs, so that path is gone entirely: with
/// the app closed, the helper answers with a clear error and touches nothing.
///
/// Claude Desktop cannot speak HTTP to a local MCP server (its configuration schema
/// admits only `command`/`args`/`env`, and custom connectors are dialled from
/// Anthropic's servers, which cannot reach loopback), so this stdio helper stays.
#[cfg(feature = "mcp")]
pub async fn start_server() {
    tracing::info!("Starting Manual MCP Server (Async JSON-RPC over Stdio)...");

    let token = std::env::var(crate::infrastructure::mcp_token::TOKEN_ENV_VAR).ok();
    if token.is_none() {
        tracing::warn!(
            "MCP: no token in the environment; re-copy the configuration from BiblioGenius settings"
        );
    }

    let mut upstream = discover_running_app().await;
    match &upstream {
        Some(base) => tracing::info!("MCP: proxying requests to running app at {}", base),
        None => tracing::info!("MCP: app not detected yet, will retry on the first tool call"),
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

                        let response = dispatch(
                            req,
                            input,
                            is_notification,
                            &mut upstream,
                            &http,
                            token.as_deref(),
                        )
                        .await;

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

/// Route a single request to the running app, rediscovering it when needed.
///
/// Handshake methods (`initialize`, `tools/list`) are answered locally: they are
/// static and carry no library data, so the assistant can connect and list the tools
/// even before the app is open. Only data-bearing calls need the app, and those fail
/// with an actionable message when it is closed.
#[cfg(feature = "mcp")]
async fn dispatch(
    req: JsonRpcRequest,
    raw_line: &str,
    is_notification: bool,
    upstream: &mut Option<String>,
    http: &Option<reqwest::Client>,
    token: Option<&str>,
) -> Option<JsonRpcResponse> {
    if is_notification {
        tracing::debug!("Received notification: {}", req.method);
        return None;
    }

    let id = req.id.clone();

    if let Some(result) = handshake_result(&req) {
        return Some(success_response(id, result));
    }

    let Some(client) = http else {
        return Some(error_response(
            id,
            "MCP helper could not build an HTTP client.",
        ));
    };
    let Some(token) = token else {
        return Some(error_response(
            id,
            "MCP token missing: re-copy the configuration from BiblioGenius settings.",
        ));
    };

    // The app may have been started after the assistant spawned this helper, so a
    // missing upstream is retried rather than remembered as a permanent failure.
    if upstream.is_none() {
        *upstream = discover_running_app().await;
    }
    let Some(base) = upstream.clone() else {
        return Some(error_response(
            id,
            "BiblioGenius is not reachable: open the app so MCP can read your library.",
        ));
    };

    match proxy_to_app(client, &base, token, raw_line).await {
        Ok(result) => result,
        Err(e) => {
            // Drop the stale base so the next call rediscovers a restarted (or
            // port-drifted) app instead of retrying a dead address forever.
            *upstream = None;
            tracing::warn!("MCP: proxy to app failed ({})", e);
            Some(error_response(
                id,
                format!("BiblioGenius is not reachable: {}", e),
            ))
        }
    }
}

/// Answer the two static handshake methods without touching any data source.
/// Returns `None` for every other method.
#[cfg(feature = "mcp")]
fn handshake_result(req: &JsonRpcRequest) -> Option<Value> {
    match req.method.as_str() {
        "initialize" => Some(initialize_result(req.params.as_ref())),
        "tools/list" => Some(tools_list_result()),
        _ => None,
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
        "initialize" => initialize_result(req.params.as_ref()),
        "tools/list" => tools_list_result(),
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

    Some(success_response(req.id, result))
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

    fn request(method: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: None,
            id: Some(json!(1)),
        }
    }

    #[test]
    fn handshake_is_answered_without_the_app() {
        // The helper no longer opens the database, so a closed app must not prevent
        // the assistant from connecting and listing the tools. Only data-bearing
        // calls need the app.
        assert!(handshake_result(&request("initialize")).is_some());
        let tools = handshake_result(&request("tools/list")).expect("tool list");
        assert_eq!(tools["tools"].as_array().expect("array").len(), 2);
        assert!(handshake_result(&request("tools/call")).is_none());
    }

    #[tokio::test]
    async fn a_missing_token_fails_cleanly_without_touching_anything() {
        // A stale configuration (copied before the token existed) must produce an
        // actionable JSON-RPC error, not a panic and not a database access.
        let mut upstream = Some("http://127.0.0.1:1".to_string());
        let http = Some(reqwest::Client::new());
        let response = dispatch(
            request("tools/call"),
            r#"{"jsonrpc":"2.0","method":"tools/call","id":1}"#,
            false,
            &mut upstream,
            &http,
            None,
        )
        .await
        .expect("an error response, not silence");

        let message = response.error.expect("error").message;
        assert!(
            message.contains("re-copy the configuration"),
            "the message must tell the user what to do: {message}"
        );
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
