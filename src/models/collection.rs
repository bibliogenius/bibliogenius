use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "collections")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String, // UUID
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub created_at: String, // String for SQLite datetime usually or DateTimeUtc
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::collection_book::Entity")]
    CollectionBook,
}

impl Related<super::collection_book::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::CollectionBook.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
