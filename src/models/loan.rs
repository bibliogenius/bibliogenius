use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "loans")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub copy_id: i32,
    pub contact_id: i32,
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

impl ActiveModelBehavior for ActiveModel {}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoanDto {
    pub id: Option<i32>,
    pub copy_id: i32,
    pub contact_id: i32,
    pub library_id: i32,
    pub loan_date: String,
    pub due_date: String,
    pub return_date: Option<String>,
    pub status: Option<String>,
    pub notes: Option<String>,
}
