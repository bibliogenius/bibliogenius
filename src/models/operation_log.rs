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
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
