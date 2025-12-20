use crate::models::operation_log;
use sea_orm::*;
use serde_json::Value;

pub async fn log_operation(
    db: &DatabaseConnection,
    entity_type: &str,
    entity_id: i32,
    operation: &str,
    payload: Option<Value>,
) -> Result<(), DbErr> {
    let log = operation_log::ActiveModel {
        entity_type: Set(entity_type.to_owned()),
        entity_id: Set(entity_id),
        operation: Set(operation.to_owned()),
        payload: Set(payload.map(|v| v.to_string())),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    operation_log::Entity::insert(log).exec(db).await?;
    Ok(())
}
pub mod processor;
