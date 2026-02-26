use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "operation_log")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub entity_type: String,
    pub entity_id: i32,
    pub operation: String,
    pub payload: Option<String>,
    #[sea_orm(default_value = "pending")]
    pub status: String, // pending, applied, failed, skipped
    pub error_message: Option<String>,
    #[sea_orm(default_value = "0")]
    pub pinned: i32,
    #[sea_orm(default_value = "local")]
    pub source: String, // "local" or "device:<id>"
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
