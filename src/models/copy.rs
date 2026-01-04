use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "copies")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub book_id: i32,
    pub library_id: i32,
    pub acquisition_date: Option<String>,
    pub notes: Option<String>,
    /// Availability status of this physical copy.
    /// Valid values:
    /// - `available`: On shelf, can be loaned
    /// - `loaned`: Currently lent to someone (has active Loan)
    /// - `borrowed`: Borrowed from another library (P2P)
    /// - `lost`: Copy is lost
    /// - `wanted`: Wishlist - don't own yet
    pub status: String,
    pub is_temporary: bool,
    pub created_at: String,
    pub updated_at: String,
    /// Price of this specific copy (EUR). Used by bookseller profile.
    /// If set, this overrides the book's default price.
    /// If NULL, the price from the parent book is used.
    pub price: Option<f64>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::book::Entity",
        from = "Column::BookId",
        to = "super::book::Column::Id"
    )]
    Book,
    #[sea_orm(
        belongs_to = "super::library::Entity",
        from = "Column::LibraryId",
        to = "super::library::Column::Id"
    )]
    Library,
}

impl Related<super::book::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Book.def()
    }
}

impl Related<super::library::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Library.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
