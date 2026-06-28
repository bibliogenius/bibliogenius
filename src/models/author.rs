use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "authors")]
pub struct Model {
    /// Stable cross-device primary key (UUID v7); stored in the `uuid` column
    /// (ADR-044 Addendum A). Minted by `before_save` when not provided.
    #[sea_orm(primary_key, auto_increment = false, column_name = "uuid")]
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
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
        if insert && self.id.is_not_set() {
            self.id = Set(crate::utils::uuid_gen::new_uuid_v7());
        }
        Ok(self)
    }
}
