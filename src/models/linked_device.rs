use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "linked_devices")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    #[sea_orm(column_type = "Binary(BlobSize::Blob(None))")]
    pub ed25519_public_key: Vec<u8>,
    #[sea_orm(column_type = "Binary(BlobSize::Blob(None))")]
    pub x25519_public_key: Vec<u8>,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
    pub relay_write_token: Option<String>,
    pub last_synced: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
