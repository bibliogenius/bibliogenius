use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "collection_books")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String, // UUID
    pub collection_id: String,
    #[sea_orm(indexed)]
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub status: String, // 'owned', 'wanted', 'ignored'
    pub cover_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::collection::Entity",
        from = "Column::CollectionId",
        to = "super::collection::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Collection,
}

impl Related<super::collection::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Collection.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
