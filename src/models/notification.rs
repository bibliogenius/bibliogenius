//! SeaORM entity for the notifications table (activity feed).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "notifications")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub event_type: String,
    pub category: String,
    pub title: String,
    pub body: Option<String>,
    pub ref_type: Option<String>,
    pub ref_id: Option<String>,
    pub read_at: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
