use axum::{Json, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
pub struct ChatRequest {
    pub message: String,
}

#[derive(Serialize)]
pub struct ChatResponse {
    pub text: String,
    pub intent: Option<String>,
    pub data: Option<Value>,
}

pub async fn chat_handler(Json(payload): Json<ChatRequest>) -> impl IntoResponse {
    let message = payload.message.to_lowercase();

    let (text, intent, data) =
        if message.contains("search") || message.contains("find") || message.contains("lookup") {
            // Extract query (very basic)
            let query = message
                .replace("search for", "")
                .replace("search", "")
                .replace("find", "")
                .replace("lookup", "")
                .trim()
                .to_string();

            if query.is_empty() {
                ("What would you like to search for?".to_string(), None, None)
            } else {
                (
                    format!("I can help you find books about '{}'.", query),
                    Some("SEARCH_PREVIEW".to_string()),
                    Some(serde_json::json!({ "query": query })),
                )
            }
        } else if message.contains("hello") || message.contains("hi") {
            (
                "Hello! I'm BiblioGenius. I can help you find books or manage your library."
                    .to_string(),
                None,
                None,
            )
        } else {
            (
                "I'm not sure how to help with that yet. Try asking me to 'search for Rust'."
                    .to_string(),
                None,
                None,
            )
        };

    Json(ChatResponse { text, intent, data })
}
