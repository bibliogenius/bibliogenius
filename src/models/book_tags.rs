use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "book_tags")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub book_id: i32,
    #[sea_orm(primary_key, auto_increment = false)]
    pub tag_id: i32,
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
        belongs_to = "super::tag::Entity",
        from = "Column::TagId",
        to = "super::tag::Column::Id",
        on_update = "Cascade",
        on_delete = "Cascade"
    )]
    Tag,
}

impl ActiveModelBehavior for ActiveModel {}
