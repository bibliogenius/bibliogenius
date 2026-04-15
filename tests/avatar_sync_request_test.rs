//! Unit tests for `handle_avatar_sync_request` (ADR-025).
//!
//! The handler is the responder-side bridge between the E2EE relay envelope
//! and the local `installation_profile.avatar_config` + `library_config.name`
//! values. These tests lock the contract: response shape, null handling when
//! either field is absent, and payload tolerance (the request carries no
//! input, so any `{}` shape must be accepted).

use rust_lib_app::crypto::envelope::ClearMessage;
use rust_lib_app::db;
use rust_lib_app::infrastructure::AppState;
use rust_lib_app::models::{installation_profile, library_config};
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use serde_json::{Value, json};

async fn setup() -> AppState {
    let db = db::init_db("sqlite::memory:")
        .await
        .expect("init_db in memory");
    AppState::new(db)
}

fn request_message(payload: Value) -> ClearMessage {
    ClearMessage {
        message_type: "avatar_sync_request".to_string(),
        payload,
        timestamp: chrono::Utc::now().timestamp(),
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: Some(uuid::Uuid::new_v4().to_string()),
        reply_to_mailbox: None,
        reply_to_write_token: None,
    }
}

/// Migrations seed a default installation_profile / library_config at id=1.
/// Tests update those rows in place instead of inserting duplicates.
async fn seed_profile(db: &DatabaseConnection, avatar_config: Option<&str>) {
    let existing = installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
        .expect("load installation_profile");
    let mut active: installation_profile::ActiveModel = existing
        .expect("installation_profile row seeded by migration 008")
        .into();
    active.avatar_config = Set(avatar_config.map(|s| s.to_owned()));
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    active
        .update(db)
        .await
        .expect("update installation_profile");
}

async fn seed_library(db: &DatabaseConnection, name: &str) {
    let existing = library_config::Entity::find_by_id(1)
        .one(db)
        .await
        .expect("load library_config");
    let mut active: library_config::ActiveModel = existing
        .expect("library_config row seeded by migration 001")
        .into();
    active.name = Set(name.to_owned());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    active.update(db).await.expect("update library_config");
}

/// Remove the migration-seeded installation_profile so handler sees no row.
async fn clear_profile(db: &DatabaseConnection) {
    installation_profile::Entity::delete_by_id(1)
        .exec(db)
        .await
        .expect("delete installation_profile");
}

/// Remove the migration-seeded library_config so handler sees no row.
async fn clear_library(db: &DatabaseConnection) {
    library_config::Entity::delete_by_id(1)
        .exec(db)
        .await
        .expect("delete library_config");
}

#[tokio::test]
async fn empty_profile_returns_null_fields() {
    let state = setup().await;
    // Strip the migration-seeded rows to exercise the "no row" path.
    clear_profile(state.db()).await;
    clear_library(state.db()).await;
    let msg = request_message(json!({}));

    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert!(
        resp.get("avatar_config")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "avatar_config must be null when no installation_profile row exists",
    );
    assert!(
        resp.get("library_name")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "library_name must be null when no library_config row exists",
    );
}

#[tokio::test]
async fn returns_avatar_when_set() {
    let state = setup().await;
    let avatar = r#"{"style":"adventurer","seed":"alice"}"#;
    seed_profile(state.db(), Some(avatar)).await;

    let msg = request_message(json!({}));
    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert_eq!(resp["avatar_config"]["style"], "adventurer");
    assert_eq!(resp["avatar_config"]["seed"], "alice");
}

#[tokio::test]
async fn returns_library_name_when_set() {
    let state = setup().await;
    seed_library(state.db(), "Bibliothèque de Federico").await;

    let msg = request_message(json!({}));
    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert_eq!(resp["library_name"], "Bibliothèque de Federico");
}

#[tokio::test]
async fn returns_both_when_both_set() {
    let state = setup().await;
    seed_profile(state.db(), Some(r#"{"style":"bottts","seed":"mac"}"#)).await;
    seed_library(state.db(), "Mac Library").await;

    let msg = request_message(json!({}));
    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert_eq!(resp["avatar_config"]["style"], "bottts");
    assert_eq!(resp["library_name"], "Mac Library");
}

#[tokio::test]
async fn null_avatar_with_library_name_set() {
    let state = setup().await;
    seed_profile(state.db(), None).await;
    seed_library(state.db(), "Naked Library").await;

    let msg = request_message(json!({}));
    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert!(
        resp["avatar_config"].is_null(),
        "avatar_config must be null when the column is NULL",
    );
    assert_eq!(resp["library_name"], "Naked Library");
}

#[tokio::test]
async fn empty_payload_is_accepted() {
    // ADR-025: request payload is `{}` — handler must not require any field.
    let state = setup().await;
    seed_profile(state.db(), Some(r#"{"style":"identicon","seed":"x"}"#)).await;

    let msg = request_message(json!({}));
    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert_eq!(resp["avatar_config"]["style"], "identicon");
}

#[tokio::test]
async fn malformed_avatar_json_is_treated_as_null() {
    // Defensive: if a corrupted avatar_config sneaks into the DB, the responder
    // must not panic and must surface it as null rather than echo garbage.
    let state = setup().await;
    seed_profile(state.db(), Some("not-json {")).await;

    let msg = request_message(json!({}));
    let resp = rust_lib_app::api::e2ee::handle_avatar_sync_request(&state, &msg).await;

    assert!(
        resp["avatar_config"].is_null(),
        "malformed avatar_config must degrade to null, not crash",
    );
}
