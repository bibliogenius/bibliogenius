use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
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
    /// Stable cross-device identifier. Generated on insert by
    /// `before_save`; backfilled on existing rows by migration 078.
    #[serde(default)]
    pub uuid: String,
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
