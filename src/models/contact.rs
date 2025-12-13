use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "contacts")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub r#type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
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

impl ActiveModelBehavior for ActiveModel {}

// DTO for API responses
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
            notes: model.notes,
            user_id: model.user_id,
            library_owner_id: model.library_owner_id,
            is_active: model.is_active,
        }
    }
}
