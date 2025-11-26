use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "authors")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::book::Entity")]
    Book,
}

impl Related<super::book::Entity> for Entity {
    fn to() -> RelationDef {
        super::book_authors::Relation::Book.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::book_authors::Relation::Author.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}
