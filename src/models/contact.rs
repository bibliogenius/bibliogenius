use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "contacts")]
pub struct Model {
    /// Stable cross-device primary key (UUID v7); stored in the `uuid` column
    /// (ADR-044 Addendum A). Minted by `before_save` when not provided.
    #[sea_orm(primary_key, auto_increment = false, column_name = "uuid")]
    pub id: String,
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
    pub library_owner_id: i32,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(
        belongs_to = "super::library::Entity",
        from = "Column::LibraryOwnerId",
        to = "super::library::Column::Id"
    )]
    Library,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::library::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Library.def()
    }
}

#[async_trait::async_trait]
impl ActiveModelBehavior for ActiveModel {
    async fn before_save<C>(mut self, _db: &C, insert: bool) -> Result<Self, DbErr>
    where
        C: ConnectionTrait,
    {
        if insert && self.id.is_not_set() {
            self.id = Set(crate::utils::uuid_gen::new_uuid_v7());
        }
        Ok(self)
    }
}

// DTO for API responses
#[derive(Debug, Serialize, Deserialize)]
pub struct ContactDto {
    pub id: Option<String>,
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
    pub library_owner_id: i32,
    pub is_active: bool,
}

impl From<Model> for ContactDto {
    fn from(model: Model) -> Self {
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
            library_owner_id: model.library_owner_id,
            is_active: model.is_active,
        }
    }
}
