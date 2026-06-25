use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "authors")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
    /// Stable cross-device identifier (ST-03). Generated on insert by
    /// `before_save`; backfilled on existing rows by migration 078.
    #[serde(default)]
    pub uuid: String,
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

#[async_trait::async_trait]
impl ActiveModelBehavior for ActiveModel {
    async fn before_save<C>(mut self, _db: &C, insert: bool) -> Result<Self, DbErr>
    where
        C: ConnectionTrait,
    {
        if insert && self.uuid.is_not_set() {
            self.uuid = Set(crate::utils::uuid_gen::new_uuid_v7());
        }
        Ok(self)
    }
}
