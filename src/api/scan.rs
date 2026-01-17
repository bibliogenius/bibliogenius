use axum::{
    Json,
    extract::{Multipart, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::DatabaseConnection;
use serde_json::json;
use std::fs;

pub async fn scan_image(
    State(_db): State<DatabaseConnection>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        if field.name() == Some("file") {
            let data = match field.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            };

            // Save to temp file
            let temp_path = format!("/tmp/scan_{}.jpg", uuid::Uuid::new_v4());
            if let Err(e) = fs::write(&temp_path, &data) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to save image: {}", e) })),
                )
                    .into_response();
            }

            // Call scanner module
            let result = crate::modules::scanner::scan_image(&temp_path);

            // Cleanup
            let _ = fs::remove_file(&temp_path);

            match result {
                Ok(text) => return (StatusCode::OK, Json(json!({ "text": text }))).into_response(),
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e })),
                    )
                        .into_response();
                }
            }
        }
    }

    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "No file uploaded" })),
    )
        .into_response()
}
