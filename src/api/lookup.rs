use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::DatabaseConnection;

#[derive(serde::Deserialize)]
pub struct LookupParams {
    pub lang: Option<String>,
}

pub async fn lookup_book(
    State(db): State<DatabaseConnection>,
    Path(isbn): Path<String>,
    axum::extract::Query(params): axum::extract::Query<LookupParams>,
) -> impl IntoResponse {
    match crate::services::lookup_service::lookup_metadata_by_isbn(
        &db,
        &isbn,
        params.lang.as_deref(),
    )
    .await
    {
        Ok(Some(metadata)) => (StatusCode::OK, Json(metadata)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Book not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}
