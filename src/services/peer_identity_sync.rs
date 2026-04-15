//! Peer identity drift repair (ADR-030).
//!
//! A peer's `library_uuid` is declared authoritatively by the peer itself
//! and carried in every `library_manifest_response`. Historical pairing
//! flows could leave the local `peers.library_uuid` stale after a remote
//! identity rotation (the emitter omitted the field in the relay payload,
//! or the receiver's update path predated the current code). The current
//! three-site update model (`peer::connect`, `peer::receive_connection_request`,
//! `relay_poller::handle_connection_request`) keeps new pairings coherent,
//! but it does not reconcile peers that were already in the stale state.
//!
//! This module owns the single write path used to reconcile a peer's
//! `library_uuid` with a value freshly learned from a cryptographically
//! verified manifest. It deliberately stays minimal: no peer-books purge
//! (the enclosing manifest sync is about to rewrite them by `peer_id`, and
//! a purge would flash an empty library in the UI), no event emission
//! (callers log context; observers rely on the downstream manifest sync).

use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};

use crate::models::peer;

/// Overwrite `peers.library_uuid` for a peer with a value learned from a
/// cryptographically verified source (E2EE manifest, ed25519-signed
/// envelope).
///
/// - Returns `Ok(false)` when the stored value already equals `new_uuid`
///   (idempotent no-op; caller may skip any downstream work).
/// - Returns `Ok(true)` when the row was updated.
/// - Returns `Err(DbErr::RecordNotFound)` if `peer_id` is unknown.
///
/// The caller is responsible for trust: only invoke this with a uuid read
/// from a payload whose envelope signature verified against the peer's
/// stored `public_key` (ed25519). Relay forwarders MUST NOT be trusted for
/// this value in isolation.
pub async fn persist_peer_library_uuid(
    db: &DatabaseConnection,
    peer_id: i32,
    new_uuid: &str,
) -> Result<bool, sea_orm::DbErr> {
    let Some(existing) = peer::Entity::find_by_id(peer_id).one(db).await? else {
        return Err(sea_orm::DbErr::RecordNotFound(format!(
            "peer {peer_id} not found"
        )));
    };

    if existing.library_uuid.as_deref() == Some(new_uuid) {
        return Ok(false);
    }

    let mut active: peer::ActiveModel = existing.into();
    active.library_uuid = Set(Some(new_uuid.to_owned()));
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    active.update(db).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use sea_orm::Set;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:")
            .await
            .expect("init_db in memory")
    }

    async fn create_peer(db: &DatabaseConnection, library_uuid: Option<&str>) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let p = peer::ActiveModel {
            name: Set("Test peer".to_owned()),
            url: Set(format!("http://peer-{}.local", uuid::Uuid::new_v4())),
            library_uuid: Set(library_uuid.map(String::from)),
            key_exchange_done: Set(false),
            connection_status: Set("accepted".to_owned()),
            auto_approve: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert peer");
        p.id
    }

    #[tokio::test]
    async fn persist_updates_stale_uuid() {
        let db = setup().await;
        let peer_id = create_peer(&db, Some("old-uuid")).await;

        let changed = persist_peer_library_uuid(&db, peer_id, "new-uuid")
            .await
            .unwrap();
        assert!(changed, "returned bool must report the row was updated");

        let reloaded = peer::Entity::find_by_id(peer_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.library_uuid.as_deref(), Some("new-uuid"));
    }

    #[tokio::test]
    async fn persist_is_idempotent_on_identical_value() {
        let db = setup().await;
        let peer_id = create_peer(&db, Some("same-uuid")).await;

        let changed = persist_peer_library_uuid(&db, peer_id, "same-uuid")
            .await
            .unwrap();
        assert!(
            !changed,
            "matching value must return false so callers can skip downstream work",
        );
    }

    #[tokio::test]
    async fn persist_populates_previously_null_uuid() {
        let db = setup().await;
        let peer_id = create_peer(&db, None).await;

        let changed = persist_peer_library_uuid(&db, peer_id, "fresh-uuid")
            .await
            .unwrap();
        assert!(changed);

        let reloaded = peer::Entity::find_by_id(peer_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.library_uuid.as_deref(), Some("fresh-uuid"));
    }

    #[tokio::test]
    async fn persist_fails_on_unknown_peer() {
        let db = setup().await;
        let err = persist_peer_library_uuid(&db, 999_999, "whatever")
            .await
            .unwrap_err();
        assert!(matches!(err, sea_orm::DbErr::RecordNotFound(_)));
    }
}
