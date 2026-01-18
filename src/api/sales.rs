use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Local;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::services::sale_service::{self, SaleFilter};

/// Request body for creating a sale
#[derive(Debug, Deserialize)]
pub struct CreateSaleRequest {
    pub copy_id: i32,
    pub contact_id: Option<i32>,
    /// Default to 1 if not provided (single library mode for FFI)
    pub library_id: Option<i32>,
    /// Default to current datetime if not provided
    pub sale_date: Option<String>,
    pub sale_price: f64,
    pub notes: Option<String>,
}

/// Query parameters for listing sales
#[derive(Debug, Deserialize)]
pub struct ListSalesQuery {
    pub library_id: Option<i32>,
    pub status: Option<String>,
    pub contact_id: Option<i32>,
}

/// Response for sale statistics
#[derive(Debug, Serialize)]
pub struct SalesStatsResponse {
    pub total_sales: i64,
    pub completed_sales: i64,
    pub total_revenue: f64,
    pub average_price: f64,
}

/// POST /api/sales - Record a new sale
pub async fn create_sale(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<CreateSaleRequest>,
) -> impl IntoResponse {
    // Default sale_date to now if not provided
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let dto = crate::models::sale::SaleDto {
        id: None,
        copy_id: payload.copy_id,
        contact_id: payload.contact_id,
        library_id: payload.library_id.unwrap_or(1),
        sale_date: payload.sale_date.unwrap_or(now),
        sale_price: payload.sale_price,
        status: None,
        notes: payload.notes,
    };

    match sale_service::record_sale(&db, dto).await {
        Ok(sale) => (
            StatusCode::CREATED,
            Json(json!({
                "success": true,
                "sale": sale
            })),
        )
            .into_response(),
        Err(sale_service::ServiceError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "error": "Copy not found"
            })),
        )
            .into_response(),
        Err(sale_service::ServiceError::InvalidState(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "error": msg
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": format!("{:?}", e)
            })),
        )
            .into_response(),
    }
}

/// GET /api/sales - List sales with optional filters
pub async fn list_sales(
    State(db): State<DatabaseConnection>,
    Query(params): Query<ListSalesQuery>,
) -> impl IntoResponse {
    let filter = SaleFilter {
        library_id: params.library_id,
        status: params.status,
        contact_id: params.contact_id,
    };

    match sale_service::list_sales(&db, filter).await {
        Ok(sales) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "sales": sales,
                "count": sales.len()
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": format!("{:?}", e)
            })),
        )
            .into_response(),
    }
}

/// GET /api/sales/:id - Get sale details (not implemented yet, optional)
pub async fn get_sale(
    State(_db): State<DatabaseConnection>,
    Path(_id): Path<i32>,
) -> impl IntoResponse {
    // TODO: Implement if needed
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "success": false,
            "error": "Not implemented yet"
        })),
    )
}

/// DELETE /api/sales/:id - Cancel a sale
pub async fn cancel_sale(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    match sale_service::cancel_sale(&db, id).await {
        Ok(sale) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "sale": sale
            })),
        )
            .into_response(),
        Err(sale_service::ServiceError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "error": "Sale not found"
            })),
        )
            .into_response(),
        Err(sale_service::ServiceError::InvalidState(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "error": msg
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": format!("{:?}", e)
            })),
        )
            .into_response(),
    }
}

/// GET /api/statistics/sales - Get sales statistics
pub async fn get_sales_statistics(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // Fetch all statistics in parallel
    let total_sales = sale_service::count_sales(&db).await.unwrap_or(0);
    let completed_sales = sale_service::count_completed_sales(&db).await.unwrap_or(0);
    let total_revenue = sale_service::calculate_total_revenue(&db)
        .await
        .unwrap_or(0.0);
    let average_price = sale_service::calculate_average_price(&db)
        .await
        .unwrap_or(0.0);

    let stats = SalesStatsResponse {
        total_sales,
        completed_sales,
        total_revenue,
        average_price,
    };

    (StatusCode::OK, Json(stats)).into_response()
}
