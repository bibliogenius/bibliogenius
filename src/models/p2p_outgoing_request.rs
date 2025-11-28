use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "p2p_outgoing_requests")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub to_peer_id: i32,
    pub book_isbn: String,
    pub book_title: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::models::peer::Entity",
        from = "Column::ToPeerId",
        to = "crate::models::peer::Column::Id"
    )]
    Peer,
}

impl Related<crate::models::peer::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Peer.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
