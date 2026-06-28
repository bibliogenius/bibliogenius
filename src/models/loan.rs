use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "loans")]
pub struct Model {
    /// Stable cross-device primary key (UUID v7); stored in the `uuid` column
    /// (ADR-044 Addendum A). Minted by `before_save` when not provided.
    #[sea_orm(primary_key, auto_increment = false, column_name = "uuid")]
    pub id: String,
    pub copy_id: String,
    pub contact_id: String,
    pub library_id: i32,
    pub loan_date: String,
    pub due_date: String,
    pub return_date: Option<String>,
    pub status: String, // 'active', 'returned', 'overdue', 'lost'
    pub notes: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::copy::Entity",
        from = "Column::CopyId",
        to = "super::copy::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Copy,
    #[sea_orm(
        belongs_to = "super::contact::Entity",
        from = "Column::ContactId",
        to = "super::contact::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Contact,
}

impl Related<super::copy::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Copy.def()
    }
}

impl Related<super::contact::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Contact.def()
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

#[derive(Debug, Serialize, Deserialize)]
pub struct LoanDto {
    pub id: Option<String>,
    pub copy_id: String,
    pub contact_id: String,
    pub library_id: i32,
    pub loan_date: String,
    pub due_date: String,
    pub return_date: Option<String>,
    pub status: Option<String>,
    pub notes: Option<String>,
}
