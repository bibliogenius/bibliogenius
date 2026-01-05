use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "collection_books")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub collection_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub book_id: i32,
    pub added_at: String,
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
    #[sea_orm(
        belongs_to = "super::book::Entity",
        from = "Column::BookId",
        to = "super::book::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Book,
}

impl Related<super::collection::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Collection.def()
    }
}

impl Related<super::book::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Book.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
