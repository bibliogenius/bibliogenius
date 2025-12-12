use crate::genie::service::GenieService;
use axum::{response::IntoResponse, Json};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ChatRequest {
    pub text: String,
}

pub async fn chat(Json(payload): Json<ChatRequest>) -> impl IntoResponse {
    let response = GenieService::process_input(&payload.text);
    Json(response)
}
