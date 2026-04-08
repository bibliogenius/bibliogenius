use crate::models::{
    contact::{self as contact_model, Entity as Contact},
    peer, peer_book,
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Serialize, Deserialize)]
pub struct ContactDto {
    pub id: Option<i32>,
    pub r#type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub street_address: Option<String>,
    pub postal_code: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub notes: Option<String>,
    pub user_id: Option<i32>,
    pub library_owner_id: Option<i32>,
    pub is_active: bool,
    /// True when this contact's peer owns the requested book (book_isbn query param).
    pub has_book: bool,
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
            street_address: model.street_address,
            postal_code: model.postal_code,
            city: model.city,
            country: model.country,
            latitude: model.latitude,
            longitude: model.longitude,
            notes: model.notes,
            user_id: model.user_id,
            library_owner_id: Some(model.library_owner_id),
            is_active: model.is_active,
            has_book: false,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ContactsQuery {
    pub library_id: Option<i32>,
    pub r#type: Option<String>,
    /// When provided, annotates each contact with `has_book = true` if the
    /// contact's peer owns a book with this ISBN.
    pub book_isbn: Option<String>,
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

    if let Some(contact_type) = &params.r#type {
        query = query.filter(contact_model::Column::Type.eq(contact_type.clone()));
    }

    let contacts = match query.all(&db).await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Database error: {}", e)})),
            )
                .into_response();
        }
    };

    // Resolve which peers own the requested book (by ISBN), if provided.
    let peer_names_with_book: HashSet<String> = match &params.book_isbn {
        None => HashSet::new(),
        Some(isbn) => {
            let matching_books = peer_book::Entity::find()
                .filter(peer_book::Column::Isbn.eq(isbn.as_str()))
                .all(&db)
                .await
                .unwrap_or_default();
            let peer_ids: Vec<i32> = matching_books.iter().map(|b| b.peer_id).collect();
            if peer_ids.is_empty() {
                HashSet::new()
            } else {
                peer::Entity::find()
                    .filter(peer::Column::Id.is_in(peer_ids))
                    .all(&db)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| p.name)
                    .collect()
            }
        }
    };

    let contact_dtos: Vec<ContactDto> = contacts
        .into_iter()
        .map(|c| {
            let has_book = peer_names_with_book.contains(&c.name);
            ContactDto {
                has_book,
                ..ContactDto::from(c)
            }
        })
        .collect();

    let total = contact_dtos.len();
    Json(serde_json::json!({
        "contacts": contact_dtos,
        "total": total
    }))
    .into_response()
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

    // Resolve library_owner_id: FK references libraries(id)
    let library_owner_id = match contact_dto.library_owner_id {
        Some(id) => id,
        None => match crate::utils::library_helpers::resolve_library_id(&db).await {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("No library found: {}", e)})),
                )
                    .into_response();
            }
        },
    };

    let new_contact = contact_model::ActiveModel {
        r#type: Set(contact_dto.r#type),
        name: Set(contact_dto.name),
        first_name: Set(contact_dto.first_name),
        email: Set(contact_dto.email),
        phone: Set(contact_dto.phone),
        address: Set(contact_dto.address),
        street_address: Set(contact_dto.street_address),
        postal_code: Set(contact_dto.postal_code),
        city: Set(contact_dto.city),
        country: Set(contact_dto.country),
        latitude: Set(contact_dto.latitude),
        longitude: Set(contact_dto.longitude),
        notes: Set(contact_dto.notes),
        user_id: Set(contact_dto.user_id),
        library_owner_id: Set(library_owner_id),
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
        active_model.street_address = Set(contact_dto.street_address);
        active_model.postal_code = Set(contact_dto.postal_code);
        active_model.city = Set(contact_dto.city);
        active_model.country = Set(contact_dto.country);
        active_model.latitude = Set(contact_dto.latitude);
        active_model.longitude = Set(contact_dto.longitude);
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

#[cfg(test)]
#[allow(clippy::needless_update)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Set, Statement};

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::infrastructure::db::run_migrations(&db)
            .await
            .unwrap();
        // Seed minimal user + library so contact FK (library_owner_id) is satisfied.
        let _: sea_orm::ExecResult = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO users (username, password_hash, role, created_at, updated_at) \
                 VALUES ('test', 'x', 'user', datetime('now'), datetime('now'))"
                    .to_owned(),
            ))
            .await
            .unwrap();
        let _: sea_orm::ExecResult = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO libraries (name, owner_id, created_at, updated_at) \
                 VALUES ('Test Library', 1, datetime('now'), datetime('now'))"
                    .to_owned(),
            ))
            .await
            .unwrap();
        db
    }

    /// Insert a peer and a matching Library contact, then verify that the
    /// has_book lookup returns true when the peer owns the given ISBN.
    #[tokio::test(flavor = "multi_thread")]
    async fn has_book_true_when_peer_owns_isbn() {
        let db = setup().await;
        let now = chrono::Utc::now().to_rfc3339();

        let saved_peer = peer::ActiveModel {
            name: Set("Alice".to_string()),
            url: Set("http://alice.local:3000".to_string()),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // Library contact whose name matches the peer
        contact_model::ActiveModel {
            r#type: Set("Library".to_string()),
            name: Set("Alice".to_string()),
            library_owner_id: Set(1), // SQLite FK not enforced
            is_active: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // Peer owns this book
        peer_book::ActiveModel {
            peer_id: Set(saved_peer.id),
            remote_book_id: Set(1),
            title: Set("Test Book".to_string()),
            isbn: Set(Some("9780123456789".to_string())),
            synced_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // Replicate the has_book lookup from list_contacts
        let matching = peer_book::Entity::find()
            .filter(peer_book::Column::Isbn.eq("9780123456789"))
            .all(&db)
            .await
            .unwrap();
        let peer_ids: Vec<i32> = matching.iter().map(|b| b.peer_id).collect();
        let names: HashSet<String> = peer::Entity::find()
            .filter(peer::Column::Id.is_in(peer_ids))
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();

        assert!(
            names.contains("Alice"),
            "Alice's peer owns the ISBN - should be annotated"
        );
    }

    /// When no peer_book matches the ISBN, the lookup returns an empty set.
    #[tokio::test(flavor = "multi_thread")]
    async fn has_book_empty_when_no_matching_peer_book() {
        let db = setup().await;
        let now = chrono::Utc::now().to_rfc3339();

        peer::ActiveModel {
            name: Set("Bob".to_string()),
            url: Set("http://bob.local:3000".to_string()),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // No peer_book inserted for this ISBN
        let matching = peer_book::Entity::find()
            .filter(peer_book::Column::Isbn.eq("9780000000000"))
            .all(&db)
            .await
            .unwrap();

        assert!(matching.is_empty(), "No peer owns this ISBN");
    }

    /// Library contacts linked to a deleted peer must be deactivated,
    /// so they no longer appear in the borrow dialog.
    #[tokio::test(flavor = "multi_thread")]
    async fn library_contact_deactivated_after_peer_deletion() {
        let db = setup().await;
        let now = chrono::Utc::now().to_rfc3339();

        let saved_peer = peer::ActiveModel {
            name: Set("Carol".to_string()),
            url: Set("http://carol.local:3000".to_string()),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        contact_model::ActiveModel {
            r#type: Set("Library".to_string()),
            name: Set("Carol".to_string()),
            library_owner_id: Set(1),
            is_active: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        // Simulate delete_peer: remove peer, then deactivate associated Library contact
        peer::Entity::delete_by_id(saved_peer.id)
            .exec(&db)
            .await
            .unwrap();
        let _ = contact_model::Entity::update_many()
            .filter(contact_model::Column::Name.eq("Carol"))
            .filter(contact_model::Column::Type.eq("Library"))
            .col_expr(
                contact_model::Column::IsActive,
                sea_orm::sea_query::Expr::value(false),
            )
            .col_expr(
                contact_model::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .exec(&db)
            .await;

        let still_active = Contact::find()
            .filter(contact_model::Column::IsActive.eq(true))
            .filter(contact_model::Column::Name.eq("Carol"))
            .all(&db)
            .await
            .unwrap();

        assert!(
            still_active.is_empty(),
            "Library contact must be inactive after peer deletion"
        );
    }
}
