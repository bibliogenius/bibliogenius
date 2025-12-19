use crate::models::book;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    params: Option<Value>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    id: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct JsonRpcError {
    code: i32,
    message: String,
    data: Option<Value>,
}

pub async fn start_server(db: DatabaseConnection) {
    tracing::info!("Starting Manual MCP Server (Async JSON-RPC over Stdio)...");

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

                        // Handle the request
                        if let Some(response) = handle_request(req, &db, is_notification).await {
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

async fn handle_request(
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
                                "query": { "type": "string", "description": "Search query (title or partial match)" }
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

                        // Perform search in DB
                        let books = book::Entity::find()
                            .filter(book::Column::Title.contains(query))
                            .limit(5)
                            .all(db)
                            .await
                            .unwrap_or_default();

                        let book_list: Vec<String> = books
                            .into_iter()
                            .map(|b| {
                                format!(
                                    "- {} ({}): {}",
                                    b.title,
                                    b.publication_year.unwrap_or(0),
                                    b.summary.unwrap_or("No summary".to_string())
                                )
                            })
                            .collect();

                        if book_list.is_empty() {
                            serde_json::json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("No books found matching '{}'", query)
                                    }
                                ]
                            })
                        } else {
                            serde_json::json!({
                                "content": [
                                    {
                                        "type": "text",
                                        "text": format!("Found {} books:\n{}", book_list.len(), book_list.join("\n"))
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
