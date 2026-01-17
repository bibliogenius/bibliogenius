use crate::models::contact::{self as contact_model, Entity as Contact};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ContactDto {
    pub id: Option<i32>,
    pub r#type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub notes: Option<String>,
    pub user_id: Option<i32>,
    pub library_owner_id: Option<i32>,
    pub is_active: bool,
}

impl From<contact_model::Model> for ContactDto {
    fn from(model: contact_model::Model) -> Self {
        Self {
            id: Some(model.id),
            r#type: model.r#type,
            name: model.name,
            first_name: model.first_name,
            email: model.email,
            phone: model.phone,
            address: model.address,
            notes: model.notes,
            user_id: model.user_id,
            library_owner_id: Some(model.library_owner_id),
            is_active: model.is_active,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ContactsQuery {
    pub library_id: Option<i32>,
    pub r#type: Option<String>,
}

// List contacts with optional filters
pub async fn list_contacts(
    State(db): State<DatabaseConnection>,
    Query(params): Query<ContactsQuery>,
) -> impl IntoResponse {
    // Start with only active contacts
    let mut query = Contact::find().filter(contact_model::Column::IsActive.eq(true));

    if let Some(library_id) = params.library_id {
        query = query.filter(contact_model::Column::LibraryOwnerId.eq(library_id));
    }

    if let Some(contact_type) = params.r#type {
        query = query.filter(contact_model::Column::Type.eq(contact_type));
    }

    match query.all(&db).await {
        Ok(contacts) => {
            let contact_dtos: Vec<ContactDto> =
                contacts.into_iter().map(ContactDto::from).collect();
            Json(serde_json::json!({
                "contacts": contact_dtos,
                "total": contact_dtos.len()
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

// Get single contact
pub async fn get_contact(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    match Contact::find_by_id(id).one(&db).await {
        Ok(Some(contact)) => {
            let contact_dto = ContactDto::from(contact);
            Json(serde_json::json!({"contact": contact_dto})).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Contact not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Database error: {}", e)})),
        )
            .into_response(),
    }
}

// Create contact
pub async fn create_contact(
    State(db): State<DatabaseConnection>,
    Json(contact_dto): Json<ContactDto>,
) -> impl IntoResponse {
    let now = chrono::Utc::now().to_rfc3339();

    let new_contact = contact_model::ActiveModel {
        r#type: Set(contact_dto.r#type),
        name: Set(contact_dto.name),
        first_name: Set(contact_dto.first_name),
        email: Set(contact_dto.email),
        phone: Set(contact_dto.phone),
        address: Set(contact_dto.address),
        notes: Set(contact_dto.notes),
        user_id: Set(contact_dto.user_id),
        library_owner_id: Set(contact_dto.library_owner_id.unwrap_or(1)),
        is_active: Set(contact_dto.is_active),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_contact.insert(&db).await {
        Ok(model) => {
            let contact_dto = ContactDto::from(model);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "contact": contact_dto,
                    "message": "Contact created successfully"
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to create contact: {}", e)})),
        )
            .into_response(),
    }
}

// Update contact
pub async fn update_contact(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
    Json(contact_dto): Json<ContactDto>,
) -> impl IntoResponse {
    let contact = Contact::find_by_id(id).one(&db).await.unwrap_or(None);

    if let Some(contact) = contact {
        let mut active_model: contact_model::ActiveModel = contact.into();
        let now = chrono::Utc::now().to_rfc3339();

        active_model.r#type = Set(contact_dto.r#type);
        active_model.name = Set(contact_dto.name);
        active_model.first_name = Set(contact_dto.first_name);
        active_model.email = Set(contact_dto.email);
        active_model.phone = Set(contact_dto.phone);
        active_model.address = Set(contact_dto.address);
        active_model.notes = Set(contact_dto.notes);
        active_model.user_id = Set(contact_dto.user_id);
        if let Some(lid) = contact_dto.library_owner_id {
            active_model.library_owner_id = Set(lid);
        }
        active_model.is_active = Set(contact_dto.is_active);
        active_model.updated_at = Set(now);

        match active_model.update(&db).await {
            Ok(model) => {
                let contact_dto = ContactDto::from(model);
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "contact": contact_dto,
                        "message": "Contact updated successfully"
                    })),
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to update contact: {}", e)})),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Contact not found"})),
        )
            .into_response()
    }
}

// Delete contact (soft delete)
pub async fn delete_contact(
    State(db): State<DatabaseConnection>,
    Path(id): Path<i32>,
) -> impl IntoResponse {
    let contact = Contact::find_by_id(id).one(&db).await.unwrap_or(None);

    if let Some(contact) = contact {
        let mut active_model: contact_model::ActiveModel = contact.into();
        active_model.is_active = Set(false);
        active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

        match active_model.update(&db).await {
            Ok(_) => (
                StatusCode::OK,
                Json(serde_json::json!({"message": "Contact deleted successfully"})),
            )
                .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to delete contact: {}", e)})),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Contact not found"})),
        )
            .into_response()
    }
}
