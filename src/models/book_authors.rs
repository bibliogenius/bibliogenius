use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "book_authors")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub book_id: i32,
    #[sea_orm(primary_key, auto_increment = false)]
    pub author_id: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::book::Entity",
        from = "Column::BookId",
        to = "super::book::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Book,
    #[sea_orm(
        belongs_to = "super::author::Entity",
        from = "Column::AuthorId",
        to = "super::author::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Author,
}

impl ActiveModelBehavior for ActiveModel {}
