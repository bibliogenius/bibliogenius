use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "library_config")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub description: Option<String>,
    pub tags: String, // JSON array stored as string
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub share_location: Option<bool>,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

// DTO for API
#[derive(Debug, Serialize, Deserialize)]
pub struct LibraryConfig {
    pub name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub share_location: bool,
}

impl From<Model> for LibraryConfig {
    fn from(model: Model) -> Self {
        let tags: Vec<String> = serde_json::from_str(&model.tags).unwrap_or_default();
        Self {
            name: model.name,
            description: model.description,
            tags,
            latitude: model.latitude,
            longitude: model.longitude,
            share_location: model.share_location.unwrap_or(false),
        }
    }
}
