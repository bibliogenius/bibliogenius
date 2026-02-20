use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Singleton row storing this node's relay mailbox configuration.
/// The `id` column is constrained to 1 (CHECK constraint in migration 043).
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "my_relay_config")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i32,
    pub relay_url: String,
    pub mailbox_uuid: String,
    pub read_token: String,
    pub write_token: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
