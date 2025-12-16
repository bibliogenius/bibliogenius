//! Contact Service - Pure business logic without HTTP layer

use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};

use crate::models::contact::{self as contact_model, Entity as Contact};

/// Error type for service operations
#[derive(Debug)]
pub enum ServiceError {
    Database(String),
    NotFound,
}

impl From<sea_orm::DbErr> for ServiceError {
    fn from(e: sea_orm::DbErr) -> Self {
        ServiceError::Database(e.to_string())
    }
}

/// Contact DTO for API responses
#[derive(Debug, Clone)]
pub struct ContactDto {
    pub id: Option<i32>,
    pub contact_type: String,
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
            contact_type: model.r#type,
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

/// Filter parameters for listing contacts
#[derive(Debug, Default, Clone)]
pub struct ContactFilter {
    pub library_id: Option<i32>,
    pub contact_type: Option<String>,
}

/// List all contacts with optional filters
pub async fn list_contacts(
    db: &DatabaseConnection,
    filter: ContactFilter,
) -> Result<Vec<ContactDto>, ServiceError> {
    let mut query = Contact::find();

    if let Some(library_id) = filter.library_id {
        query = query.filter(contact_model::Column::LibraryOwnerId.eq(library_id));
    }

    if let Some(contact_type) = filter.contact_type {
        query = query.filter(contact_model::Column::Type.eq(contact_type));
    }

    let contacts = query.all(db).await?;
    Ok(contacts.into_iter().map(ContactDto::from).collect())
}

/// Get a single contact by ID
pub async fn get_contact(db: &DatabaseConnection, id: i32) -> Result<ContactDto, ServiceError> {
    let contact = Contact::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    Ok(ContactDto::from(contact))
}

/// Create a new contact
pub async fn create_contact(
    db: &DatabaseConnection,
    dto: ContactDto,
) -> Result<ContactDto, ServiceError> {
    let now = chrono::Utc::now().to_rfc3339();

    let new_contact = contact_model::ActiveModel {
        r#type: Set(dto.contact_type),
        name: Set(dto.name),
        first_name: Set(dto.first_name),
        email: Set(dto.email),
        phone: Set(dto.phone),
        address: Set(dto.address),
        notes: Set(dto.notes),
        user_id: Set(dto.user_id),
        library_owner_id: Set(dto.library_owner_id.unwrap_or(1)),
        is_active: Set(dto.is_active),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    let saved_contact = new_contact.insert(db).await?;

    Ok(ContactDto::from(saved_contact))
}

/// Update an existing contact
pub async fn update_contact(
    db: &DatabaseConnection,
    dto: ContactDto,
) -> Result<ContactDto, ServiceError> {
    let id = dto.id.ok_or(ServiceError::Database(
        "Contact ID is required for update".to_string(),
    ))?;

    let contact = Contact::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    let mut active_model: contact_model::ActiveModel = contact.into();
    let now = chrono::Utc::now().to_rfc3339();

    active_model.r#type = Set(dto.contact_type);
    active_model.name = Set(dto.name);
    active_model.first_name = Set(dto.first_name);
    active_model.email = Set(dto.email);
    active_model.phone = Set(dto.phone);
    active_model.address = Set(dto.address);
    active_model.notes = Set(dto.notes);
    active_model.user_id = Set(dto.user_id);
    if let Some(lid) = dto.library_owner_id {
        active_model.library_owner_id = Set(lid);
    }
    active_model.is_active = Set(dto.is_active);
    active_model.updated_at = Set(now);

    let model = active_model.update(db).await?;
    Ok(ContactDto::from(model))
}

/// Delete a contact (soft delete)
pub async fn delete_contact(db: &DatabaseConnection, id: i32) -> Result<(), ServiceError> {
    let contact = Contact::find_by_id(id)
        .one(db)
        .await?
        .ok_or(ServiceError::NotFound)?;

    let mut active_model: contact_model::ActiveModel = contact.into();
    active_model.is_active = Set(false);
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    active_model.update(db).await?;
    Ok(())
}

/// Count total contacts
pub async fn count_contacts(db: &DatabaseConnection) -> Result<i64, ServiceError> {
    use sea_orm::PaginatorTrait;
    let count = Contact::find().count(db).await?;
    Ok(count as i64)
}
