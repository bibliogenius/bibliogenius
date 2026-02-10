use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "peer_gamification_stats")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub peer_id: i32,
    pub library_name: String,
    pub collector_level: i32,
    pub collector_current: i32,
    pub reader_level: i32,
    pub reader_current: i32,
    pub lender_level: i32,
    pub lender_current: i32,
    pub cataloguer_level: i32,
    pub cataloguer_current: i32,
    pub synced_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::peer::Entity",
        from = "Column::PeerId",
        to = "super::peer::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Peer,
}

impl Related<super::peer::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Peer.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
