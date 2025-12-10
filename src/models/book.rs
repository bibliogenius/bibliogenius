use sea_orm::entity::prelude::*;
use sea_orm::{NotSet, Set};
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
    pub dewey_decimal: Option<String>,
    pub lcc: Option<String>,
    pub subjects: Option<String>, // JSON array
    pub marc_record: Option<String>,
    pub cataloguing_notes: Option<String>,
    pub source_data: Option<String>,
    pub shelf_position: Option<i32>,
    #[sea_orm(default_value = "to_read")]
    pub reading_status: String,
    pub finished_reading_at: Option<String>,
    pub started_reading_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

// ... (Relation enum and Related impls omit for brevity) ...
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dewey_decimal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subjects: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marc_record: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cataloguing_notes: Option<String>,
    pub source_data: Option<String>,
    pub shelf_position: Option<i32>,
    pub reading_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_reading_at: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_reading_at: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
}

impl From<Model> for Book {
    fn from(model: Model) -> Self {
        let subjects: Option<Vec<String>> = model
            .subjects
            .map(|s| serde_json::from_str(&s).unwrap_or_default());

        Self {
            id: Some(model.id),
            title: model.title,
            isbn: model.isbn,
            summary: model.summary,
            publisher: model.publisher,
            publication_year: model.publication_year,
            dewey_decimal: model.dewey_decimal,
            lcc: model.lcc,
            subjects,
            marc_record: model.marc_record,
            cataloguing_notes: model.cataloguing_notes,
            source_data: model.source_data,
            shelf_position: model.shelf_position,
            reading_status: Some(model.reading_status),
            finished_reading_at: Some(model.finished_reading_at),
            started_reading_at: Some(model.started_reading_at),
            source: Some("Local".to_string()),
            author: None,    // TODO: Fetch from relation
            cover_url: None, // TODO: Derive from ISBN or store in DB
        }
    }
}

impl From<Book> for ActiveModel {
    fn from(book: Book) -> Self {
        Self {
            id: book.id.map_or(NotSet, Set),
            title: Set(book.title),
            isbn: Set(book.isbn),
            summary: Set(book.summary),
            publisher: Set(book.publisher),
            publication_year: Set(book.publication_year),
            dewey_decimal: Set(book.dewey_decimal),
            lcc: Set(book.lcc),
            subjects: Set(book
                .subjects
                .map(|s| serde_json::to_string(&s).unwrap_or_default())),
            marc_record: Set(book.marc_record),
            cataloguing_notes: Set(book.cataloguing_notes),
            source_data: Set(book.source_data),
            shelf_position: Set(book.shelf_position),
            reading_status: book.reading_status.map_or(NotSet, Set),
            finished_reading_at: book.finished_reading_at.map_or(NotSet, Set),
            started_reading_at: book.started_reading_at.map_or(NotSet, Set),
            created_at: NotSet,
            updated_at: NotSet,
        }
    }
}
