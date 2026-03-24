//! SeaORM entity for the book_notes table.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "book_notes")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub book_id: i32,
    pub content: String,
    pub page: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::models::book::Entity",
        from = "Column::BookId",
        to = "crate::models::book::Column::Id"
    )]
    Book,
}

impl Related<crate::models::book::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Book.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
