use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "tags")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub name: String,
    pub parent_id: Option<i32>,
    #[serde(default)]
    pub path: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::book::Entity")]
    Book,
    #[sea_orm(belongs_to = "Entity", from = "Column::ParentId", to = "Column::Id")]
    Parent,
}

impl Related<super::book::Entity> for Entity {
    fn to() -> RelationDef {
        super::book_tags::Relation::Book.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::book_tags::Relation::Tag.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}
