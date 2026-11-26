use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "contacts")]
pub struct Model {
    /// Stable cross-device primary key (UUID v7); stored in the `uuid` column
    /// (ADR-044 Addendum A). Minted by `before_save` when not provided.
    #[sea_orm(primary_key, auto_increment = false, column_name = "uuid")]
    pub id: String,
    pub r#type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub street_address: Option<String>,
    pub postal_code: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub notes: Option<String>,
    pub user_id: Option<i32>,
    pub library_owner_id: i32,
    pub is_active: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::user::Entity",
        from = "Column::UserId",
        to = "super::user::Column::Id"
    )]
    User,
    #[sea_orm(
        belongs_to = "super::library::Entity",
        from = "Column::LibraryOwnerId",
        to = "super::library::Column::Id"
    )]
    Library,
}

impl Related<super::user::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl Related<super::library::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Library.def()
    }
}

impl Model {
    /// Display name used wherever a loan or sale names the contact: "First Last",
    /// or the last name alone when there is no first name. Mirrors the app's
    /// `Contact.fullName` so every screen shows the same name.
    pub fn full_name(&self) -> String {
        match self.first_name.as_deref() {
            Some(first) if !first.is_empty() && !self.name.is_empty() => {
                format!("{first} {}", self.name)
            }
            _ => self.name.clone(),
        }
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

// DTO for API responses
#[derive(Debug, Serialize, Deserialize)]
pub struct ContactDto {
    pub id: Option<String>,
    pub r#type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub street_address: Option<String>,
    pub postal_code: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub notes: Option<String>,
    pub user_id: Option<i32>,
    pub library_owner_id: i32,
    pub is_active: bool,
}

impl From<Model> for ContactDto {
    fn from(model: Model) -> Self {
        Self {
            id: Some(model.id),
            r#type: model.r#type,
            name: model.name,
            first_name: model.first_name,
            email: model.email,
            phone: model.phone,
            address: model.address,
            street_address: model.street_address,
            postal_code: model.postal_code,
            city: model.city,
            country: model.country,
            latitude: model.latitude,
            longitude: model.longitude,
            notes: model.notes,
            user_id: model.user_id,
            library_owner_id: model.library_owner_id,
            is_active: model.is_active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(first_name: Option<&str>, name: &str) -> Model {
        Model {
            id: "c".to_string(),
            r#type: "person".to_string(),
            name: name.to_string(),
            first_name: first_name.map(str::to_string),
            email: None,
            phone: None,
            address: None,
            street_address: None,
            postal_code: None,
            city: None,
            country: None,
            latitude: None,
            longitude: None,
            notes: None,
            user_id: None,
            library_owner_id: 1,
            is_active: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn full_name_puts_the_first_name_before_the_last_name() {
        assert_eq!(
            contact(Some("Jeanne"), "Dupont").full_name(),
            "Jeanne Dupont"
        );
    }

    #[test]
    fn full_name_falls_back_to_the_last_name() {
        assert_eq!(contact(None, "Dupont").full_name(), "Dupont");
        assert_eq!(contact(Some(""), "Dupont").full_name(), "Dupont");
    }

    #[test]
    fn full_name_is_empty_when_the_last_name_is_empty() {
        // Same rule as the app's `Contact.fullName`: no first name alone.
        assert_eq!(contact(Some("Jeanne"), "").full_name(), "");
    }
}
