use rust_lib_app::db;
use rust_lib_app::domain::{CreateLinkedDeviceInput, LinkedDeviceRepository};
use rust_lib_app::infrastructure::SeaOrmLinkedDeviceRepository;

async fn setup_repo() -> SeaOrmLinkedDeviceRepository {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("Failed to init DB");
    SeaOrmLinkedDeviceRepository::new(db)
}

fn sample_input(name: &str) -> CreateLinkedDeviceInput {
    CreateLinkedDeviceInput {
        name: name.to_string(),
        ed25519_public_key: vec![0xAA; 32],
        x25519_public_key: vec![0xBB; 32],
        relay_url: None,
        mailbox_id: None,
        relay_write_token: None,
    }
}

#[tokio::test]
async fn test_create_and_find_by_id() {
    let repo = setup_repo().await;

    let device = repo
        .create(sample_input("MacBook Pro"))
        .await
        .expect("create failed");

    assert!(device.id.is_some());
    assert_eq!(device.name, "MacBook Pro");
    assert_eq!(device.ed25519_public_key, vec![0xAA; 32]);
    assert_eq!(device.x25519_public_key, vec![0xBB; 32]);
    assert!(device.created_at.is_some());

    let found = repo
        .find_by_id(device.id.unwrap())
        .await
        .expect("find_by_id failed");
    assert!(found.is_some());
    let found = found.unwrap();
    assert_eq!(found.name, "MacBook Pro");
    assert_eq!(found.ed25519_public_key, vec![0xAA; 32]);
}

#[tokio::test]
async fn test_find_all_empty() {
    let repo = setup_repo().await;
    let devices = repo.find_all().await.expect("find_all failed");
    assert!(devices.is_empty());
}

#[tokio::test]
async fn test_find_all_multiple() {
    let repo = setup_repo().await;
    repo.create(sample_input("iPhone")).await.unwrap();
    repo.create(sample_input("iPad")).await.unwrap();

    let devices = repo.find_all().await.expect("find_all failed");
    assert_eq!(devices.len(), 2);
}

#[tokio::test]
async fn test_find_by_id_not_found() {
    let repo = setup_repo().await;
    let result = repo.find_by_id(999).await.expect("find_by_id failed");
    assert!(result.is_none());
}

#[tokio::test]
async fn test_create_with_relay_fields() {
    let repo = setup_repo().await;

    let input = CreateLinkedDeviceInput {
        name: "Remote iPad".to_string(),
        ed25519_public_key: vec![0x11; 32],
        x25519_public_key: vec![0x22; 32],
        relay_url: Some("https://relay.example.com".to_string()),
        mailbox_id: Some("mbx-abc-123".to_string()),
        relay_write_token: Some("wt-secret".to_string()),
    };

    let device = repo.create(input).await.expect("create failed");
    assert_eq!(
        device.relay_url.as_deref(),
        Some("https://relay.example.com")
    );
    assert_eq!(device.mailbox_id.as_deref(), Some("mbx-abc-123"));
    assert_eq!(device.relay_write_token.as_deref(), Some("wt-secret"));
    assert!(device.last_synced.is_none());
}

#[tokio::test]
async fn test_update_last_synced() {
    let repo = setup_repo().await;
    let device = repo.create(sample_input("MacBook")).await.unwrap();
    let id = device.id.unwrap();

    repo.update_last_synced(id, "2026-02-26T10:00:00Z")
        .await
        .expect("update_last_synced failed");

    let updated = repo.find_by_id(id).await.unwrap().unwrap();
    assert_eq!(updated.last_synced.as_deref(), Some("2026-02-26T10:00:00Z"));
}

#[tokio::test]
async fn test_update_last_synced_not_found() {
    let repo = setup_repo().await;
    let result = repo.update_last_synced(999, "2026-02-26T10:00:00Z").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_delete() {
    let repo = setup_repo().await;
    let device = repo.create(sample_input("Old Phone")).await.unwrap();
    let id = device.id.unwrap();

    repo.delete(id).await.expect("delete failed");

    let found = repo.find_by_id(id).await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_delete_not_found() {
    let repo = setup_repo().await;
    let result = repo.delete(999).await;
    assert!(result.is_err());
}
