//! SeaORM implementation of LinkedDeviceRepository

use async_trait::async_trait;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};

use crate::domain::{CreateLinkedDeviceInput, DomainError, LinkedDevice, LinkedDeviceRepository};
use crate::models::linked_device::{ActiveModel, Entity as LinkedDeviceEntity};

/// SeaORM-based implementation of LinkedDeviceRepository
pub struct SeaOrmLinkedDeviceRepository {
    db: DatabaseConnection,
}

impl SeaOrmLinkedDeviceRepository {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

fn model_to_domain(m: crate::models::linked_device::Model) -> LinkedDevice {
    LinkedDevice {
        id: Some(m.id),
        name: m.name,
        ed25519_public_key: m.ed25519_public_key,
        x25519_public_key: m.x25519_public_key,
        relay_url: m.relay_url,
        mailbox_id: m.mailbox_id,
        relay_write_token: m.relay_write_token,
        last_synced: m.last_synced,
        created_at: Some(m.created_at),
    }
}

#[async_trait]
impl LinkedDeviceRepository for SeaOrmLinkedDeviceRepository {
    async fn find_all(&self) -> Result<Vec<LinkedDevice>, DomainError> {
        let devices = LinkedDeviceEntity::find().all(&self.db).await?;
        Ok(devices.into_iter().map(model_to_domain).collect())
    }

    async fn find_by_id(&self, id: i32) -> Result<Option<LinkedDevice>, DomainError> {
        let result = LinkedDeviceEntity::find_by_id(id).one(&self.db).await?;
        Ok(result.map(model_to_domain))
    }

    async fn create(&self, input: CreateLinkedDeviceInput) -> Result<LinkedDevice, DomainError> {
        let new_device = ActiveModel {
            name: Set(input.name),
            ed25519_public_key: Set(input.ed25519_public_key),
            x25519_public_key: Set(input.x25519_public_key),
            relay_url: Set(input.relay_url),
            mailbox_id: Set(input.mailbox_id),
            relay_write_token: Set(input.relay_write_token),
            ..Default::default()
        };

        let result = new_device.insert(&self.db).await?;
        Ok(model_to_domain(result))
    }

    async fn update_last_synced(&self, id: i32, timestamp: &str) -> Result<(), DomainError> {
        let existing = LinkedDeviceEntity::find_by_id(id)
            .one(&self.db)
            .await?
            .ok_or(DomainError::NotFound)?;

        let mut active: ActiveModel = existing.into();
        active.last_synced = Set(Some(timestamp.to_owned()));
        active.update(&self.db).await?;

        Ok(())
    }

    async fn delete(&self, id: i32) -> Result<(), DomainError> {
        let result = LinkedDeviceEntity::delete_by_id(id).exec(&self.db).await?;

        if result.rows_affected == 0 {
            return Err(DomainError::NotFound);
        }

        Ok(())
    }
}
