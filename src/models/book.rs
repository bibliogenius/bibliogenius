use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "books")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub title: String,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::copy::Entity")]
    Copies,
}

impl Related<super::author::Entity> for Entity {
    fn to() -> RelationDef {
        super::book_authors::Relation::Author.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::book_authors::Relation::Book.def().rev())
    }
}

impl Related<super::tag::Entity> for Entity {
    fn to() -> RelationDef {
        super::book_tags::Relation::Tag.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::book_tags::Relation::Book.def().rev())
    }
}

impl ActiveModelBehavior for ActiveModel {}

// DTO for API responses
#[derive(Debug, Serialize, Deserialize)]
pub struct Book {
    pub id: Option<i32>,
    pub title: String,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
}

impl From<Model> for Book {
    fn from(model: Model) -> Self {
        Self {
            id: Some(model.id),
            title: model.title,
            isbn: model.isbn,
            summary: model.summary,
            publisher: model.publisher,
            publication_year: model.publication_year,
        }
    }
}
