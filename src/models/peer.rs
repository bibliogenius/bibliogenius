use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "peers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    #[sea_orm(unique)]
    pub url: String,
    /// Stable UUID for P2P deduplication (survives IP changes)
    pub library_uuid: Option<String>,
    pub public_key: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    #[sea_orm(default_value = "false")]
    pub auto_approve: bool,
    pub last_seen: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
