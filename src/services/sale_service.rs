//! Sale Service - Business logic for sales transactions (bookseller profile)
//! Mirrored from loan_service.rs

use chrono::Local;
use sea_orm::*;
use std::collections::HashMap;

use crate::models::book::Entity as Book;
use crate::models::contact::Entity as Contact;
use crate::models::copy::{self, Entity as Copy};
use crate::models::sale::{self, Entity as Sale, SaleDto};

/// Error type for service operations
#[derive(Debug)]
pub enum ServiceError {
    Database(String),
    NotFound,
    InvalidState(String),
}

impl From<sea_orm::DbErr> for ServiceError {
    fn from(e: sea_orm::DbErr) -> Self {
        ServiceError::Database(e.to_string())
    }
}

/// Enriched sale with related data
#[derive(Debug, Clone, serde::Serialize)] // Added Serialize for API responses
pub struct SaleWithDetails {
    pub id: i32,
    pub copy_id: i32,
    pub contact_id: Option<i32>,
    pub library_id: i32,
    pub sale_date: String,
    pub sale_price: f64,
    pub status: String,
    pub notes: Option<String>,
    pub contact_name: Option<String>, // None if no contact
    pub book_title: String,
}

/// Filter parameters for listing sales
#[derive(Debug, Default, Clone)]
pub struct SaleFilter {
    pub library_id: Option<i32>,
    pub status: Option<String>,
    pub contact_id: Option<i32>,
}

/// List all sales with related contact and book info
pub async fn list_sales(
    db: &DatabaseConnection,
    filter: SaleFilter,
) -> Result<Vec<SaleWithDetails>, ServiceError> {
    let mut condition = Condition::all();

    if let Some(library_id) = filter.library_id {
        condition = condition.add(sale::Column::LibraryId.eq(library_id));
    }

    if let Some(status) = filter.status {
        condition = condition.add(sale::Column::Status.eq(status));
    }

    if let Some(contact_id) = filter.contact_id {
        condition = condition.add(sale::Column::ContactId.eq(contact_id));
    }

    let sales_with_contacts = Sale::find()
        .filter(condition)
        .order_by_desc(sale::Column::SaleDate)
        .find_also_related(Contact)
        .all(db)
        .await?;

    // Collect copy IDs to fetch books
    let copy_ids: Vec<i32> = sales_with_contacts.iter().map(|(s, _)| s.copy_id).collect();

    // Fetch copies with books
    let mut copy_book_map: HashMap<i32, String> = HashMap::new();

    if !copy_ids.is_empty() {
        let copies_with_books = Copy::find()
            .filter(copy::Column::Id.is_in(copy_ids))
            .find_also_related(Book)
            .all(db)
            .await?;

        for (copy, book) in copies_with_books {
            if let Some(book) = book {
                copy_book_map.insert(copy.id, book.title);
            }
        }
    }

    let result: Vec<SaleWithDetails> = sales_with_contacts
        .into_iter()
        .map(|(sale, contact)| {
            let contact_name = contact.map(|c| c.name);
            let book_title = copy_book_map
                .get(&sale.copy_id)
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string());

            SaleWithDetails {
                id: sale.id,
                copy_id: sale.copy_id,
                contact_id: sale.contact_id,
                library_id: sale.library_id,
                sale_date: sale.sale_date,
                sale_price: sale.sale_price,
                status: sale.status,
                notes: sale.notes,
                contact_name,
                book_title,
            }
        })
        .collect();

    Ok(result)
}

/// Record a new sale
pub async fn record_sale(
    db: &DatabaseConnection,
    dto: SaleDto,
) -> Result<sale::Model, ServiceError> {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Check if copy exists and is available
    let _copy = Copy::find_by_id(dto.copy_id) // Prefixed with _ to avoid warning
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    // Note: We don't check copy status for sales - a book can be sold in any state
    // The copy status can be updated to "sold" if needed, but that's configurable

    // 2. Create Sale
    let new_sale = sale::ActiveModel {
        copy_id: Set(dto.copy_id),
        contact_id: Set(dto.contact_id),
        library_id: Set(dto.library_id),
        sale_date: Set(dto.sale_date),
        sale_price: Set(dto.sale_price),
        status: Set("completed".to_owned()),
        notes: Set(dto.notes),
        created_at: Set(now.clone()),
        updated_at: Set(now.clone()),
        ..Default::default()
    };

    let saved_sale = new_sale.insert(db).await?;

    // 3. Update Copy status to 'sold' and set sold_at
    let mut copy_active: copy::ActiveModel = _copy.into();
    copy_active.status = Set("sold".to_owned());
    copy_active.sold_at = Set(Some(now));
    copy_active.update(db).await?;

    Ok(saved_sale)
}

/// Cancel a sale
pub async fn cancel_sale(db: &DatabaseConnection, id: i32) -> Result<sale::Model, ServiceError> {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    // 1. Find Sale
    let sale = Sale::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    if sale.status == "cancelled" {
        return Err(ServiceError::InvalidState(
            "Sale is already cancelled".to_string(),
        ));
    }

    // 2. Update Sale
    let mut sale_active: sale::ActiveModel = sale.clone().into();
    sale_active.status = Set("cancelled".to_owned());
    sale_active.updated_at = Set(now.clone());

    let updated_sale = sale_active.update(db).await?;

    // 3. Update Copy status back to 'available' and clear sold_at
    if let Some(copy_model) = Copy::find_by_id(sale.copy_id).one(db).await? {
        let mut copy_active: copy::ActiveModel = copy_model.into();
        copy_active.status = Set("available".to_owned());
        copy_active.sold_at = Set(None);
        copy_active.updated_at = Set(now);
        copy_active.update(db).await?;
    }

    Ok(updated_sale)
}

/// Count total sales
pub async fn count_sales(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = Sale::find().count(db).await?;
    Ok(count as i64)
}

/// Count completed sales
pub async fn count_completed_sales(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    let count = Sale::find()
        .filter(sale::Column::Status.eq("completed"))
        .count(db)
        .await?;
    Ok(count as i64)
}

/// Calculate total revenue from completed sales
pub async fn calculate_total_revenue(db: &DatabaseConnection) -> Result<f64, ServiceError> {
    let sales = Sale::find()
        .filter(sale::Column::Status.eq("completed"))
        .all(db)
        .await?;

    let total: f64 = sales.iter().map(|s| s.sale_price).sum();
    Ok(total)
}

/// Calculate average sale price
pub async fn calculate_average_price(db: &DatabaseConnection) -> Result<f64, ServiceError> {
    let total = calculate_total_revenue(db).await?;
    let count = count_completed_sales(db).await?;

    if count == 0 {
        Ok(0.0)
    } else {
        Ok(total / count as f64)
    }
}
