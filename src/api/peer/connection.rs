//! Peer connection and disconnection lifecycle.

use super::*;
use crate::models::peer;
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

#[derive(Deserialize)]
pub struct ConnectRequest {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) public_key: Option<String>,
    /// Stable library UUID for P2P peer deduplication
    #[serde(default)]
    pub(crate) library_uuid: Option<String>,
    /// Ed25519 public key (hex) from the remote peer - for E2EE
    #[serde(default)]
    pub(crate) ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the remote peer - for E2EE
    #[serde(default)]
    pub(crate) x25519_public_key: Option<String>,
    /// Peer's relay hub URL
    #[serde(default)]
    pub(crate) relay_url: Option<String>,
    /// Peer's relay mailbox UUID
    #[serde(default)]
    pub(crate) mailbox_id: Option<String>,
    /// Token to write to peer's relay mailbox
    #[serde(default)]
    pub(crate) relay_write_token: Option<String>,
}

/// What the address named in an outbound pairing answered on `/api/config`,
/// all `None` when nothing trustworthy answered.
#[derive(Default)]
pub(crate) struct RemoteConfigData {
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    pub(crate) remote_name: Option<String>,
    pub(crate) library_uuid: Option<String>,
    pub(crate) ed25519_public_key: Option<String>,
    pub(crate) x25519_public_key: Option<String>,
    pub(crate) relay_url: Option<String>,
    pub(crate) mailbox_id: Option<String>,
    pub(crate) relay_write_token: Option<String>,
    pub(crate) avatar_config: Option<String>,
}

/// Stores or refreshes the peer named by an outbound pairing (QR code or
/// invitation link). Returns the row id and the URL the catalog sync uses,
/// `None` when no catalog may be pulled from the address.
pub(crate) async fn register_outbound_peer(
    db: &DatabaseConnection,
    payload: ConnectRequest,
    remote_data: RemoteConfigData,
) -> Result<(i32, Option<String>), String> {
    // Relay-only peers carry an empty URL: a placeholder keeps `peers.url`
    // unique and the row is reached through the hub.
    let is_relay_only = payload.url.is_empty();

    // The address named by the scan answered as another library (an address
    // reassigned on the LAN, two apps on one phone): nothing it said is the
    // scanned peer's, and no catalog is pulled from it. The scan itself is
    // still stored, reachable through its relay credentials.
    let scanned_key = payload
        .ed25519_public_key
        .as_deref()
        .or(payload.public_key.as_deref());
    let address_is_another_library = {
        let uuid_conflict = matches!(
            (payload.library_uuid.as_deref(), remote_data.library_uuid.as_deref()),
            (Some(a), Some(b)) if a != b
        );
        let key_conflict = matches!(
            (scanned_key, remote_data.ed25519_public_key.as_deref()),
            (Some(a), Some(b)) if a != b
        );
        uuid_conflict || key_conflict
    };
    let remote_data = if address_is_another_library {
        tracing::warn!(
            "connect: {} answered as another library, ignoring its config",
            payload.url
        );
        RemoteConfigData::default()
    } else {
        remote_data
    };
    // The address answered with the scanned library's own identity: it is
    // allowed to take that address away from a previous holder.
    let address_answered_as_scanned = !address_is_another_library
        && (remote_data.library_uuid.is_some() || remote_data.ed25519_public_key.is_some());
    let mut address_usable = !is_relay_only && !address_is_another_library;

    // Use provided name or fallback to remote name or "Unknown"
    let payload_name_given = !payload.name.is_empty();
    let name = if payload_name_given {
        payload.name
    } else {
        remote_data
            .remote_name
            .unwrap_or_else(|| "Unknown Library".to_string())
    };

    // What the scan itself declares, before any merge with the address's
    // answer: only the scan may set the keys of a row that already exists.
    let scan_ed25519 = payload
        .ed25519_public_key
        .clone()
        .or_else(|| payload.public_key.clone());
    let scan_x25519 = payload.x25519_public_key.clone();
    let scan_uuid = payload.library_uuid.clone();
    let scan_relay = (
        payload.relay_url.clone(),
        payload.mailbox_id.clone(),
        payload.relay_write_token.clone(),
    );
    let answer_uuid = remote_data.library_uuid.clone();
    let answer_ed25519 = remote_data.ed25519_public_key.clone();

    // Prefer keys from the request payload (QR/invite), fall back to ConfigResponse keys.
    // Legacy `public_key` field is used as fallback for ed25519 (backward compat).
    let ed25519_key = payload
        .ed25519_public_key
        .or(remote_data.ed25519_public_key)
        .or(payload.public_key);
    let x25519_key = payload.x25519_public_key.or(remote_data.x25519_public_key);

    // Key exchange is done if we have both keys
    let key_exchange_done = ed25519_key.is_some() && x25519_key.is_some();

    // Library UUID: prefer payload (QR/invite), fall back to remote config
    let peer_library_uuid = payload.library_uuid.or(remote_data.library_uuid);

    // Scanning a library whose pairing was ended, or adding it by its
    // address, is the owner restoring it (ADR-072 D4): the scan names it,
    // or else, when the scan names nothing, the address answers for it.
    // Never one field from each: they could be two libraries. Lifted before
    // the row is written, so the pairing never stands while its messages
    // are still refused.
    let restored = if scan_uuid.is_some() || scan_ed25519.is_some() {
        (scan_uuid.as_deref(), scan_ed25519.as_deref())
    } else {
        (answer_uuid.as_deref(), answer_ed25519.as_deref())
    };
    crate::services::hub_catalog_readers::restore_pairing(db, restored.0, restored.1).await?;

    // Translate localhost URLs to Docker service names for inter-container communication
    let scanned_url = translate_url_for_docker(&payload.url);

    // Upsert, identity first: library uuid, then ed25519 key, then the URL
    // only when the row there carries no identity that contradicts the
    // scanned one. Two libraries can take turns on one host:port (two apps
    // on one phone); a URL match used to merge the scanned library into the
    // trusted row of the other one, keys included.
    let existing = find_peer_for_outbound(
        db,
        peer_library_uuid.as_deref(),
        ed25519_key.as_deref(),
        if address_usable { &scanned_url } else { "" },
    )
    .await;

    // An address held by another trusted row changes hands only once it
    // answered as the scanned library; the previous holder then keeps its
    // identity and credentials under `relay://`. A silent address stays
    // with its holder and the scanned peer is reached through its relay
    // until a sync or an inbound pairing settles the address.
    if address_usable {
        let keep = match &existing {
            Ok(Some(found)) => Some(found.id),
            _ => None,
        };
        match other_holders(db, &scanned_url, keep).await {
            Ok(holders) if holders.is_empty() => {}
            Ok(holders) if address_answered_as_scanned => {
                if let Err(e) = release_holders(db, &scanned_url, holders).await {
                    return Err(format!("could not release {}: {e}", scanned_url));
                }
            }
            Ok(_) => {
                tracing::info!(
                    "connect: {} is held by another peer and did not answer as the scanned \
                     library, keeping it with its holder",
                    scanned_url
                );
                address_usable = false;
            }
            Err(e) => return Err(format!("could not look up {}: {e}", scanned_url)),
        }
    }

    // For relay-only peers (empty URL), use a unique placeholder to satisfy
    // the NOT NULL UNIQUE constraint on peers.url (same pattern as relay_poller).
    let docker_url = if address_usable {
        scanned_url
    } else {
        let unique_part = peer_library_uuid
            .as_deref()
            .or(ed25519_key.as_deref())
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        format!("relay://{unique_part}")
    };
    let peer_url_for_sync = docker_url.clone(); // Clone before moving into ActiveModel

    // Relay info: prefer payload, fall back to remote config
    let relay_url = payload.relay_url.or(remote_data.relay_url);
    let mailbox_id = payload.mailbox_id.or(remote_data.mailbox_id);
    let relay_write_token = payload.relay_write_token.or(remote_data.relay_write_token);

    // Set when the address answered with an identity that contradicts the
    // row the scan matched: that address is not the peer's.
    let mut answer_contradicts_row = false;
    let peer_id = match existing {
        Ok(Some(existing_peer)) => {
            // Update existing peer with new keys and info
            let peer_id = existing_peer.id;
            let old_library_uuid = existing_peer.library_uuid.clone();
            // The address's answer only fills what the scan left out when it
            // does not contradict the row; otherwise the scan alone counts.
            let answer_concords = !identity_conflicts(
                &existing_peer,
                answer_uuid.as_deref(),
                answer_ed25519.as_deref(),
            );
            answer_contradicts_row = !answer_concords;
            let (peer_library_uuid, relay_url, mailbox_id, relay_write_token, name) =
                if answer_concords {
                    (
                        peer_library_uuid,
                        relay_url,
                        mailbox_id,
                        relay_write_token,
                        name,
                    )
                } else {
                    tracing::warn!(
                        "connect: the address of peer {} answered with another identity, \
                         using the scan only",
                        peer_id
                    );
                    let name = if payload_name_given {
                        name
                    } else {
                        existing_peer.name.clone()
                    };
                    (scan_uuid, scan_relay.0, scan_relay.1, scan_relay.2, name)
                };
            let (latitude, longitude, avatar_config) = if answer_concords {
                (
                    remote_data.latitude,
                    remote_data.longitude,
                    remote_data.avatar_config.clone(),
                )
            } else {
                (None, None, None)
            };
            let mut active: peer::ActiveModel = existing_peer.into();
            active.name = Set(name);
            // Update URL (port may have changed). A placeholder never
            // replaces a LAN address: the row keeps the address it has.
            if (address_usable && answer_concords) || active.url.as_ref().starts_with("relay://") {
                active.url = Set(docker_url.clone());
            }
            // A scan that declares no identity erases none (legacy payload
            // with name and url only); a scan that does is the user's own
            // gesture and may replace the keys.
            if peer_library_uuid.is_some() {
                active.library_uuid = Set(peer_library_uuid.clone());
            }
            if let (Some(ed), Some(x)) = (&scan_ed25519, &scan_x25519) {
                active.public_key = Set(Some(ed.clone()));
                active.x25519_public_key = Set(Some(x.clone()));
                active.key_exchange_done = Set(true);
            }
            active.latitude = Set(latitude);
            active.longitude = Set(longitude);
            active.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            active.auto_approve = Set(true);
            active.connection_status = Set("accepted".to_string());
            // Store avatar config if provided
            if avatar_config.is_some() {
                active.avatar_config = Set(avatar_config);
            }
            // Store relay info if provided
            if relay_url.is_some() {
                active.relay_url = Set(relay_url);
            }
            if mailbox_id.is_some() {
                active.mailbox_id = Set(mailbox_id);
            }
            if relay_write_token.is_some() {
                active.relay_write_token = Set(relay_write_token);
                // ADR-032: fresh invitation clears any stale-token gate.
                active.relay_write_token_invalid_at = Set(None);
            }
            match active.update(db).await {
                Ok(_) => {
                    // If library_uuid changed (peer was reset/reinstalled),
                    // clear cached books - the old library no longer exists.
                    let uuid_changed = match (&old_library_uuid, &peer_library_uuid) {
                        (Some(old), Some(new)) => old != new,
                        (None, Some(_)) => false, // first time getting uuid, keep cache
                        _ => false,
                    };
                    if uuid_changed {
                        // upsert_peer_books_cache (called by background sync)
                        // handles the transition atomically: insert new, update
                        // existing, delete absent. No premature cache wipe.
                        tracing::info!(
                            "Peer {} library_uuid changed, will refresh via upsert",
                            peer_id
                        );
                    }
                    peer_id
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        _ => {
            // Insert new peer
            let peer = peer::ActiveModel {
                name: Set(name),
                url: Set(docker_url),
                library_uuid: Set(peer_library_uuid),
                public_key: Set(ed25519_key.clone()),
                x25519_public_key: Set(x25519_key),
                key_exchange_done: Set(key_exchange_done),
                latitude: Set(remote_data.latitude),
                longitude: Set(remote_data.longitude),
                relay_url: Set(relay_url),
                mailbox_id: Set(mailbox_id),
                relay_write_token: Set(relay_write_token),
                avatar_config: Set(remote_data.avatar_config),
                last_seen: Set(Some(chrono::Utc::now().to_rfc3339())),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                auto_approve: Set(true),
                ..Default::default()
            };
            match peer::Entity::insert(peer).exec(db).await {
                Ok(res) => res.last_insert_id,
                Err(e) => return Err(e.to_string()),
            }
        }
    };

    let sync_url = if address_is_another_library || answer_contradicts_row {
        None
    } else {
        Some(peer_url_for_sync)
    };
    Ok((peer_id, sync_url))
}

pub async fn connect(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<ConnectRequest>,
) -> impl IntoResponse {
    // Relay-only peers have an empty URL — skip URL validation and remote
    // config fetch in that case. All data comes from the request payload.
    let is_relay_only = payload.url.is_empty();

    // 1. Validate URL (only for LAN peers with a real HTTP URL)
    if !is_relay_only && let Err(e) = validate_url(&payload.url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Fetch remote config to get location and verify connectivity
    let (remote_data, remote_reachable) = if is_relay_only {
        // Relay-only: no remote config to fetch, all data from payload
        (RemoteConfigData::default(), false)
    } else {
        let client = get_safe_client();
        let config_url = format!("{}/api/config", payload.url.trim_end_matches('/'));
        match client.get(&config_url).send().await {
            Ok(res) => {
                if res.status().is_success() {
                    match res.json::<crate::api::setup::ConfigResponse>().await {
                        Ok(config) => {
                            let (lat, long) = if config.share_location {
                                (config.latitude, config.longitude)
                            } else {
                                (None, None)
                            };
                            let avatar = config
                                .avatar_config
                                .map(|v| serde_json::to_string(&v).unwrap_or_default());
                            (
                                RemoteConfigData {
                                    latitude: lat,
                                    longitude: long,
                                    remote_name: Some(config.library_name),
                                    library_uuid: config.library_uuid,
                                    ed25519_public_key: config.ed25519_public_key,
                                    x25519_public_key: config.x25519_public_key,
                                    relay_url: config.relay_url,
                                    mailbox_id: config.mailbox_id,
                                    relay_write_token: config.relay_write_token,
                                    avatar_config: avatar,
                                },
                                true,
                            )
                        }
                        _ => (RemoteConfigData::default(), false),
                    }
                } else {
                    (RemoteConfigData::default(), false)
                }
            }
            Err(_) => (RemoteConfigData::default(), false),
        }
    };

    // Relay handshake inputs, read before the payload moves into the upsert:
    // the scanned payload first, the address's own answer as fallback.
    let relay_url_for_handshake = payload
        .relay_url
        .clone()
        .or_else(|| remote_data.relay_url.clone());
    let mailbox_id_for_handshake = payload
        .mailbox_id
        .clone()
        .or_else(|| remote_data.mailbox_id.clone());
    let relay_write_token_for_handshake = payload
        .relay_write_token
        .clone()
        .or_else(|| remote_data.relay_write_token.clone());

    let (peer_id, peer_url_for_sync) = match register_outbound_peer(&db, payload, remote_data).await
    {
        Ok(registered) => registered,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };

    // Trigger background sync of peer catalog
    let address_answered_as_peer = peer_url_for_sync.is_some();
    if let Some(peer_url_for_sync) = peer_url_for_sync {
        let db_clone = db.clone();
        tokio::spawn(async move {
            tracing::info!("🔄 Background sync triggered for new peer {}", peer_id);
            if let Err(e) = sync_peer_internal(&db_clone, peer_id, &peer_url_for_sync).await {
                tracing::warn!("⚠️ Background sync failed for peer {}: {}", peer_id, e);
            }
        });
    }

    // If the remote peer was unreachable (no WiFi) but we have their relay
    // credentials, tell Flutter to deposit the connection_request via native
    // HTTP (Dio). reqwest+rustls fails on iOS FFI, so the deposit is handled
    // by the Flutter caller using the native HTTP stack. An address that
    // answered as another library counts as unreachable.
    let remote_reachable = remote_reachable && address_answered_as_peer;
    if !remote_reachable
        && relay_url_for_handshake.is_some()
        && mailbox_id_for_handshake.is_some()
        && relay_write_token_for_handshake.is_some()
    {
        tracing::info!("Relay: Peer unreachable, relay_deposit_needed=true (Flutter will deposit)");
        return (
            StatusCode::CREATED,
            Json(json!({
                "id": peer_id,
                "relay_deposit_needed": true
            })),
        )
            .into_response();
    }

    (StatusCode::CREATED, Json(json!({ "id": peer_id }))).into_response()
}

#[derive(Deserialize)]
pub struct IncomingConnectionRequest {
    pub(crate) name: String,
    pub(crate) url: String,
    /// Stable library UUID for P2P peer deduplication
    #[serde(default)]
    pub(crate) library_uuid: Option<String>,
    /// Ed25519 public key (hex) from the requesting peer - for E2EE
    #[serde(default)]
    pub(crate) ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the requesting peer - for E2EE
    #[serde(default)]
    pub(crate) x25519_public_key: Option<String>,
    /// Peer's relay hub URL
    #[serde(default)]
    pub(crate) relay_url: Option<String>,
    /// Peer's relay mailbox UUID
    #[serde(default)]
    pub(crate) mailbox_id: Option<String>,
    /// Token to write to peer's relay mailbox
    #[serde(default)]
    pub(crate) relay_write_token: Option<String>,
}

/// Which stored peer a LAN connection request refers to, identity first.
///
/// `library_uuid`, then the ed25519 key, then the URL. Two libraries can take
/// turns on one `host:port` (a development build and an installed build of the
/// desktop app), and matching on the URL first renamed one into the other and
/// left it with the wrong keys. A URL match is therefore only accepted when the
/// stored row carries no identity that contradicts the request.
async fn find_peer_for_connection(
    db: &DatabaseConnection,
    payload: &IncomingConnectionRequest,
) -> Result<Option<peer::Model>, sea_orm::DbErr> {
    if let Some(uuid) = &payload.library_uuid
        && let Some(found) = peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(db)
            .await?
    {
        return Ok(Some(found));
    }
    if let Some(key) = &payload.ed25519_public_key
        && let Some(found) = peer::Entity::find()
            .filter(peer::Column::PublicKey.eq(key))
            .one(db)
            .await?
    {
        return Ok(Some(found));
    }
    let by_url = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .one(db)
        .await?;
    Ok(by_url.filter(|found| !contradicts_identity(found, payload)))
}

/// The peers other than `keep` currently stored under `url`.
async fn other_holders(
    db: &DatabaseConnection,
    url: &str,
    keep: Option<i32>,
) -> Result<Vec<peer::Model>, sea_orm::DbErr> {
    Ok(peer::Entity::find()
        .filter(peer::Column::Url.eq(url))
        .all(db)
        .await?
        .into_iter()
        .filter(|holder| Some(holder.id) != keep)
        .collect())
}

/// Take `url` away from its previous holders. `peers.url` is UNIQUE, and an
/// address changes hands when two libraries take turns on one `host:port`. The
/// previous holder keeps its identity and relay credentials under the
/// `relay://` placeholder already used for relay-only peers, so it stays
/// reachable through the hub and gets a LAN address back on its next
/// discovery. Nothing is deleted.
async fn release_holders(
    db: &DatabaseConnection,
    url: &str,
    holders: Vec<peer::Model>,
) -> Result<(), sea_orm::DbErr> {
    for holder in holders {
        let placeholder = format!(
            "relay://{}",
            holder
                .library_uuid
                .clone()
                .or_else(|| holder.public_key.clone())
                .unwrap_or_else(|| format!("peer-{}", holder.id))
        );
        tracing::info!(
            "register_peer: address {} now belongs to another library, moving peer {} to {}",
            url,
            holder.id,
            placeholder
        );
        let mut active: peer::ActiveModel = holder.into();
        active.url = Set(placeholder);
        active.updated_at = Set(Utc::now().to_rfc3339());
        active.update(db).await?;
    }
    Ok(())
}

/// True when the address in the request serves the ed25519 key it claims.
///
/// This endpoint is unauthenticated and a peer's public keys are public (the
/// hub publishes them), so a request is only allowed to move or refresh a
/// trusted peer once its address has answered `/api/config` with that key.
/// That is a concordance of public values, not a proof of possession: a
/// host can serve another library's published key. Such a host can take the
/// peer's address and relay credentials (its messages stay unreadable to it,
/// but the peer becomes unreachable, and a plaintext catalog fallback would
/// read the host's books under the peer's name); it never gets the keys or
/// a stored uuid.
async fn address_hosts_identity<F, Fut>(probe: &F, payload: &IncomingConnectionRequest) -> bool
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let Some(claimed) = payload.ed25519_public_key.as_deref() else {
        return false;
    };
    probe(payload.url.clone()).await.as_deref() == Some(claimed)
}

/// Give the request's address to the peer being registered. A free address is
/// taken as is; one held by another trusted identity only once the address
/// proved it hosts the claimed identity (`already_proven` skips a second probe).
/// Returns false when the holder keeps it.
async fn claim_address<F, Fut>(
    db: &DatabaseConnection,
    payload: &IncomingConnectionRequest,
    probe: &F,
    keep: Option<i32>,
    already_proven: bool,
) -> bool
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let holders = match other_holders(db, &payload.url, keep).await {
        Ok(holders) => holders,
        Err(e) => {
            tracing::warn!("register_peer: could not look up {}: {e}", payload.url);
            return false;
        }
    };
    if holders.is_empty() {
        return true;
    }
    if !already_proven && !address_hosts_identity(probe, payload).await {
        tracing::warn!(
            "register_peer: {} did not prove it hosts the claimed identity, its holder keeps it",
            payload.url
        );
        return false;
    }
    match release_holders(db, &payload.url, holders).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("register_peer: could not release {}: {e}", payload.url);
            false
        }
    }
}

/// Ask the library at `url` who it is: its `/api/config` carries its ed25519 key.
async fn probe_ed25519_at(url: String) -> Option<String> {
    let config_url = format!("{}/api/config", url.trim_end_matches('/'));
    let response = get_safe_client()
        .get(&config_url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response
        .json::<crate::api::setup::ConfigResponse>()
        .await
        .ok()?
        .ed25519_public_key
}

/// True when the stored row and the request both declare an identity and they
/// differ: another library behind the same address, never the same peer.
fn contradicts_identity(existing: &peer::Model, payload: &IncomingConnectionRequest) -> bool {
    identity_conflicts(
        existing,
        payload.library_uuid.as_deref(),
        payload.ed25519_public_key.as_deref(),
    )
}

/// The identity rule shared by the inbound and outbound pairing paths: a
/// stored row and a declared identity conflict when either the library uuid
/// or the ed25519 key is present on both sides and differs.
fn identity_conflicts(existing: &peer::Model, uuid: Option<&str>, key: Option<&str>) -> bool {
    let uuid_conflict =
        matches!((existing.library_uuid.as_deref(), uuid), (Some(a), Some(b)) if a != b);
    let key_conflict =
        matches!((existing.public_key.as_deref(), key), (Some(a), Some(b)) if a != b);
    uuid_conflict || key_conflict
}

/// Which stored peer an outbound pairing (QR code, invitation link) refers
/// to: library uuid, then ed25519 key, then the URL when nothing stored there
/// contradicts the scanned identity. Same order as
/// [`find_peer_for_connection`] on the inbound path.
async fn find_peer_for_outbound(
    db: &DatabaseConnection,
    uuid: Option<&str>,
    key: Option<&str>,
    url: &str,
) -> Result<Option<peer::Model>, sea_orm::DbErr> {
    if let Some(uuid) = uuid
        && let Some(found) = peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(db)
            .await?
    {
        return Ok(Some(found));
    }
    if let Some(key) = key
        && let Some(found) = peer::Entity::find()
            .filter(peer::Column::PublicKey.eq(key))
            .one(db)
            .await?
    {
        return Ok(Some(found));
    }
    if url.is_empty() {
        return Ok(None);
    }
    let by_url = peer::Entity::find()
        .filter(peer::Column::Url.eq(url))
        .one(db)
        .await?;
    Ok(by_url.filter(|found| !identity_conflicts(found, uuid, key)))
}

/// True when the request carries exactly the keys already stored for the peer.
fn declares_same_identity(existing: &peer::Model, payload: &IncomingConnectionRequest) -> bool {
    matches!(
        (&existing.public_key, &payload.ed25519_public_key),
        (Some(a), Some(b)) if a == b
    ) && matches!(
        (&existing.x25519_public_key, &payload.x25519_public_key),
        (Some(a), Some(b)) if a == b
    )
}

/// Receive an incoming connection request from a remote peer.
/// Always creates/updates the peer in local SQLite and returns our E2EE keys.
/// Also forwards to the Hub (fire-and-forget) for the central directory.
pub async fn receive_connection_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingConnectionRequest>,
) -> impl IntoResponse {
    register_connection_request(db, payload, probe_ed25519_at).await
}

/// Body of [`receive_connection_request`]. `probe` answers with the ed25519 key
/// served at a URL's `/api/config`, or `None` when nothing trustworthy answers;
/// it is injected so the tests run without a network.
pub(crate) async fn register_connection_request<F, Fut>(
    db: DatabaseConnection,
    payload: IncomingConnectionRequest,
    probe: F,
) -> axum::response::Response
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    if let Err(e) = validate_url(&payload.url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    tracing::info!(
        "Peer: Received connection_request from '{}' (url='{}', e2ee={}, relay={}, library_uuid={:?})",
        payload.name,
        payload.url,
        payload.ed25519_public_key.is_some() && payload.x25519_public_key.is_some(),
        payload.relay_url.is_some(),
        payload.library_uuid
    );

    // Try forwarding to Hub (fire-and-forget for central directory).
    // Always continue to local handling regardless of hub result,
    // so the peer is created in our local SQLite and we return our E2EE keys.
    if let Ok(hub_url) = std::env::var("HUB_URL") {
        let endpoint = format!("{}/api/peers/receive_connection", hub_url);
        let client = get_safe_client();
        let _ = client
            .post(&endpoint)
            .json(&serde_json::json!({
                "name": payload.name,
                "url": payload.url,
            }))
            .send()
            .await;
    }

    // Always handle locally: create/update peer in SQLite + return our E2EE keys
    let existing = find_peer_for_connection(&db, &payload).await;

    // Load our own public keys to include in the response
    let (my_ed25519, my_x25519) = crate::api::setup::load_public_keys_from_db(&db).await;

    // Determine if peer sent E2EE keys
    let key_exchange_done =
        payload.ed25519_public_key.is_some() && payload.x25519_public_key.is_some();

    match existing {
        Ok(Some(existing_peer)) => {
            // Peer already exists - update keys, relay info, and library_uuid if provided
            let old_uuid = existing_peer.library_uuid.clone();
            let peer_id = existing_peer.id;
            let same_identity = declares_same_identity(&existing_peer, &payload);
            let address_changes = existing_peer.url != payload.url;
            let first_exchange = key_exchange_done && !existing_peer.key_exchange_done;
            // Whether the request may speak for the library the row describes.
            // The route is unauthenticated and a peer's public keys are public
            // (the hub publishes them): a row matched by its keys is only
            // rewritten, beyond the first key exchange, once the address
            // proved it serves that key. Name, uuid and cache follow the same
            // proof as the address and the relay credentials.
            let proven = if first_exchange {
                true
            } else if key_exchange_done && same_identity {
                address_hosts_identity(&probe, &payload).await
            } else {
                false
            };
            // A row that never exchanged keys has no identity to check against.
            let may_rewrite = proven || !existing_peer.key_exchange_done;
            if may_rewrite && !payload.name.is_empty() && payload.name != existing_peer.name {
                match peer::Entity::update_many()
                    .filter(peer::Column::Id.eq(peer_id))
                    .col_expr(
                        peer::Column::Name,
                        sea_orm::sea_query::Expr::value(payload.name.clone()),
                    )
                    .col_expr(
                        peer::Column::UpdatedAt,
                        sea_orm::sea_query::Expr::value(Utc::now().to_rfc3339()),
                    )
                    .exec(&db)
                    .await
                {
                    Ok(_) => tracing::info!(
                        "register_peer: updated peer {} name '{}' -> '{}'",
                        peer_id,
                        existing_peer.name,
                        payload.name
                    ),
                    Err(e) => {
                        tracing::warn!("register_peer: could not rename peer {}: {e}", peer_id)
                    }
                }
            }

            if first_exchange {
                let takes_address = !address_changes
                    || claim_address(&db, &payload, &probe, Some(peer_id), false).await;
                if !takes_address {
                    tracing::warn!(
                        "register_peer: peer {} keeps its stored address, {} was refused",
                        peer_id,
                        payload.url
                    );
                }
                let mut active: peer::ActiveModel = existing_peer.into();
                if takes_address {
                    active.url = Set(payload.url.clone()); // Update URL (port may have changed)
                }
                if payload.library_uuid.is_some() {
                    active.library_uuid = Set(payload.library_uuid.clone());
                }
                active.public_key = Set(payload.ed25519_public_key.clone());
                active.x25519_public_key = Set(payload.x25519_public_key.clone());
                active.key_exchange_done = Set(true);
                if payload.relay_url.is_some() {
                    active.relay_url = Set(payload.relay_url.clone());
                }
                if payload.mailbox_id.is_some() {
                    active.mailbox_id = Set(payload.mailbox_id.clone());
                }
                if payload.relay_write_token.is_some() {
                    active.relay_write_token = Set(payload.relay_write_token.clone());
                    // ADR-032: fresh invitation clears any stale-token gate.
                    active.relay_write_token_invalid_at = Set(None);
                }
                active.updated_at = Set(Utc::now().to_rfc3339());
                if let Err(e) = active.update(&db).await {
                    tracing::warn!("register_peer: could not update peer {}: {e}", peer_id);
                }
            } else if key_exchange_done && same_identity {
                // The same library pairs again: its address or relay mailbox may
                // have changed (reset device, recreated mailbox, new LAN port).
                // The block above never ran for a peer already exchanged, so the
                // stale credentials survived every re-pairing and the peer's
                // messages died on a dead mailbox. Keys stay as stored: this
                // endpoint is unauthenticated, a rotation must come through a
                // verified channel (ADR-026).
                if !proven {
                    tracing::warn!(
                        "register_peer: {} did not prove it hosts peer {}, refresh refused",
                        payload.url,
                        peer_id
                    );
                } else {
                    let takes_address = !address_changes
                        || claim_address(&db, &payload, &probe, Some(peer_id), true).await;
                    let mut active: peer::ActiveModel = existing_peer.into();
                    if takes_address {
                        active.url = Set(payload.url.clone());
                    }
                    if payload.relay_url.is_some() {
                        active.relay_url = Set(payload.relay_url.clone());
                    }
                    if payload.mailbox_id.is_some() {
                        active.mailbox_id = Set(payload.mailbox_id.clone());
                    }
                    if payload.relay_write_token.is_some() {
                        active.relay_write_token = Set(payload.relay_write_token.clone());
                        // ADR-032: fresh invitation clears any stale-token gate.
                        active.relay_write_token_invalid_at = Set(None);
                    }
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    match active.update(&db).await {
                        Ok(_) => tracing::info!(
                            "register_peer: refreshed address and relay credentials of peer {}",
                            peer_id
                        ),
                        Err(e) => {
                            tracing::warn!("register_peer: could not refresh peer {}: {e}", peer_id)
                        }
                    }
                }
            } else if key_exchange_done {
                tracing::warn!(
                    "register_peer: peer {} presented different keys, keeping the stored identity",
                    peer_id
                );
            }
            // Fill a missing library_uuid. A stored uuid never changes on
            // this path: the address serving a public key does not prove who
            // holds it, and ADR-053 approves hub follows by uuid. A reset peer
            // gets its new uuid through the signed manifest (ADR-030), which
            // also refreshes the cached books.
            if let Some(new_uuid) = &payload.library_uuid
                && old_uuid.is_none()
                && may_rewrite
            {
                // Update library_uuid on the peer record
                if let Err(e) = peer::Entity::update_many()
                    .filter(peer::Column::Id.eq(peer_id))
                    .col_expr(
                        peer::Column::LibraryUuid,
                        sea_orm::sea_query::Expr::value(new_uuid.clone()),
                    )
                    .exec(&db)
                    .await
                {
                    tracing::warn!(
                        "register_peer: could not update uuid of peer {}: {e}",
                        peer_id
                    );
                }
            } else if let Some(new_uuid) = &payload.library_uuid
                && old_uuid.as_deref() != Some(new_uuid.as_str())
            {
                tracing::warn!(
                    "register_peer: peer {} declared another library_uuid, keeping the stored one",
                    peer_id
                );
            }

            // Load our relay config to include in response
            let my_relay = crate::api::relay::get_my_relay_config(&db).await;

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer already exists locally",
                    "ed25519_public_key": my_ed25519,
                    "x25519_public_key": my_x25519,
                    "relay_url": my_relay.as_ref().map(|r| &r.relay_url),
                    "mailbox_id": my_relay.as_ref().map(|r| &r.mailbox_uuid),
                    "relay_write_token": my_relay.as_ref().map(|r| &r.write_token),
                })),
            )
                .into_response()
        }
        Ok(None) => {
            // A trusted row may still hold this address with another identity
            // (two libraries taking turns on one host:port). The address is
            // theirs no more once it proved it hosts the newcomer, see
            // `claim_address`; otherwise the holder keeps it, as
            // `update_peer_url` already answers for a taken address.
            if !claim_address(&db, &payload, &probe, None, false).await {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "URL already in use by another trusted peer" })),
                )
                    .into_response();
            }
            // A new peer waits for the owner when connection validation is
            // on, and always when the owner ended a pairing with this
            // library, known by its uuid or its signing key: an ex-peer must
            // not pair itself back in (ADR-072 D4).
            let pairing_ended = crate::services::hub_catalog_readers::pairing_ended(
                &db,
                payload.library_uuid.as_deref(),
                payload.ed25519_public_key.as_deref(),
            )
            .await
            .unwrap_or(true);
            let connection_status = if pairing_ended || is_connection_validation_enabled(&db).await
            {
                "pending"
            } else {
                "accepted"
            };

            let peer_name_for_notif = payload.name.clone();
            let new_peer = peer::ActiveModel {
                name: Set(payload.name),
                url: Set(payload.url),
                library_uuid: Set(payload.library_uuid),
                public_key: Set(payload.ed25519_public_key),
                x25519_public_key: Set(payload.x25519_public_key),
                key_exchange_done: Set(key_exchange_done),
                relay_url: Set(payload.relay_url),
                mailbox_id: Set(payload.mailbox_id),
                relay_write_token: Set(payload.relay_write_token),
                auto_approve: Set(connection_status == "accepted"),
                connection_status: Set(connection_status.to_string()),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };

            // Load our relay config to include in response
            let my_relay = crate::api::relay::get_my_relay_config(&db).await;

            match new_peer.insert(&db).await {
                Ok(ref inserted) => {
                    tracing::info!(
                        "Peer: Created new peer '{}' (id={}, e2ee={}, relay={}, status={})",
                        peer_name_for_notif,
                        inserted.id,
                        key_exchange_done,
                        my_relay.is_some(),
                        connection_status
                    );
                    // Signal Flutter instantly so PendingPeersProvider refreshes
                    // without waiting for the 30s fallback timer (direct LAN path has
                    // no relay_poller cycle to emit this automatically).
                    crate::services::nudge_events::bus().emit(
                        crate::services::nudge_events::NudgeEvent {
                            mailbox_id: String::new(),
                            source: crate::services::nudge_events::NudgeSource::Manual,
                        },
                    );

                    if connection_status == "pending" {
                        // Emit connection_request notification (needs user action)
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type: crate::domain::NotificationEventType::ConnectionRequest,
                                title: peer_name_for_notif.clone(),
                                body: None,
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(peer_name_for_notif),
                            },
                        )
                        .await;
                    } else {
                        // Auto-accepted: emit connection_accepted notification
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type:
                                    crate::domain::NotificationEventType::ConnectionAccepted,
                                title: peer_name_for_notif.clone(),
                                body: None,
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(peer_name_for_notif),
                            },
                        )
                        .await;
                    }

                    (
                        StatusCode::OK,
                        Json(json!({
                            "message": "Connection request saved locally",
                            "connection_status": connection_status,
                            "ed25519_public_key": my_ed25519,
                            "x25519_public_key": my_x25519,
                            "relay_url": my_relay.as_ref().map(|r| &r.relay_url),
                            "mailbox_id": my_relay.as_ref().map(|r| &r.mailbox_uuid),
                            "relay_write_token": my_relay.as_ref().map(|r| &r.write_token),
                        })),
                    )
                        .into_response()
                }
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to save peer locally: {}", e) })),
                )
                    .into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Database error: {}", e) })),
        )
            .into_response(),
    }
}

/// Notify a remote peer that we are disconnecting.
///
/// Tries E2EE first (encrypted, with relay fallback for offline peers),
/// then falls back to a plaintext HTTP POST for peers without E2EE keys.
/// Errors are logged but never propagated - disconnection is always local-first.
pub(crate) async fn notify_peer_of_disconnect(
    state: &crate::infrastructure::AppState,
    peer_model: &peer::Model,
) {
    // Send OUR library_uuid (stable identifier) + URL as fallback.
    // The remote peer will search by library_uuid first, then by URL.
    let our_library_uuid = state.identity_service.library_uuid().map(|s| s.to_string());
    let our_url = state.our_public_url();

    let payload = json!({
        "peer_url": our_url,
        "library_uuid": our_library_uuid,
        "timestamp": Utc::now().to_rfc3339(),
    });

    // Try E2EE notification (handles relay fallback internally)
    match try_send_e2ee(state, peer_model, "peer_disconnect", payload).await {
        Ok(Some(_)) => {
            info!(
                "Disconnect notification sent (E2EE) to peer {} ({})",
                peer_model.name, peer_model.id
            );
            return;
        }
        Ok(None) => {
            // E2EE not available for this peer, fall through to plaintext
        }
        Err(e) => {
            info!(
                "E2EE disconnect notification failed for peer {}: {}, trying plaintext",
                peer_model.name, e
            );
        }
    }

    // HMAC-authenticated fallback: POST /api/peers/notify-disconnect
    // Requires key_exchange_done + x25519_public_key (shared secret for HMAC).
    // If keys are not available, we skip entirely (no unauthenticated fallback).
    let our_uuid = match &our_library_uuid {
        Some(uuid) => uuid.clone(),
        None => {
            info!(
                "Disconnect: no library_uuid, skipping plaintext fallback for peer {}",
                peer_model.name
            );
            return;
        }
    };

    if !peer_model.key_exchange_done {
        info!(
            "Disconnect: key_exchange not done, skipping plaintext fallback for peer {}",
            peer_model.name
        );
        return;
    }

    let peer_x25519_hex = match &peer_model.x25519_public_key {
        Some(hex) => hex.clone(),
        None => {
            info!(
                "Disconnect: no x25519_public_key, skipping plaintext fallback for peer {}",
                peer_model.name
            );
            return;
        }
    };

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            info!(
                "Disconnect: CryptoService not initialized, skipping plaintext fallback for peer {}",
                peer_model.name
            );
            return;
        }
    };

    // Compute HMAC
    let timestamp = Utc::now().to_rfc3339();
    let peer_x25519_bytes = match hex::decode(&peer_x25519_hex) {
        Ok(b) if b.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b);
            arr
        }
        _ => {
            info!(
                "Disconnect: invalid x25519_public_key hex for peer {}",
                peer_model.name
            );
            return;
        }
    };
    let peer_pub = x25519_dalek::PublicKey::from(peer_x25519_bytes);
    let hmac = crate::crypto::key_exchange::compute_disconnect_hmac(
        crypto_service.identity().x25519_static_secret(),
        &peer_pub,
        &our_uuid,
        &timestamp,
    );

    let client = get_safe_client();
    let url = format!("{}/api/peers/notify-disconnect", peer_model.url);
    match client
        .post(&url)
        .json(&json!({
            "peer_url": our_url,
            "library_uuid": our_uuid,
            "timestamp": timestamp,
            "hmac": hex::encode(hmac),
        }))
        .send()
        .await
    {
        Ok(res) => {
            info!(
                "Disconnect notification sent (HMAC, status={}) to peer {} ({})",
                res.status(),
                peer_model.name,
                peer_model.id
            );
        }
        Err(e) => {
            info!(
                "HMAC disconnect notification failed for peer {}: {} (peer may be offline)",
                peer_model.name, e
            );
        }
    }
}

/// Receive an HMAC-authenticated disconnect notification from a remote peer.
///
/// Defense layers:
/// 1. Requires library_uuid, timestamp, and HMAC (all mandatory)
/// 2. Validates timestamp within +/-5 minutes (replay window)
/// 3. Verifies HMAC using X25519 static shared secret
/// 4. Re-handshake: asks the sender to confirm the disconnect
pub async fn receive_disconnect_notification(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<DisconnectNotification>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Require all authentication fields
    let library_uuid = match &payload.library_uuid {
        Some(uuid) if !uuid.trim().is_empty() => uuid.trim().to_string(),
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing library_uuid" })),
            )
                .into_response();
        }
    };
    let timestamp = match &payload.timestamp {
        Some(ts) if !ts.trim().is_empty() => ts.trim().to_string(),
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing timestamp" })),
            )
                .into_response();
        }
    };
    let hmac_hex = match &payload.hmac {
        Some(h) if !h.trim().is_empty() => h.trim().to_string(),
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing HMAC" })),
            )
                .into_response();
        }
    };

    // 2. Validate timestamp (must be within +/-5 minutes)
    let parsed_ts = match chrono::DateTime::parse_from_rfc3339(&timestamp) {
        Ok(ts) => ts.with_timezone(&Utc),
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid timestamp format (expected RFC3339)" })),
            )
                .into_response();
        }
    };
    let now = Utc::now();
    let drift = (now - parsed_ts).abs();
    if drift > chrono::Duration::minutes(5) {
        return (
            StatusCode::GONE,
            Json(json!({ "error": "Timestamp outside acceptable window" })),
        )
            .into_response();
    }

    // 3. Decode HMAC hex
    let hmac_bytes = match hex::decode(&hmac_hex) {
        Ok(b) if b.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&b);
            arr
        }
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid HMAC format" })),
            )
                .into_response();
        }
    };

    // 4. Find peer by library_uuid first, then URL fallback
    let peer_url = payload.peer_url.trim();
    let found_peer = match peer::Entity::find()
        .filter(peer::Column::LibraryUuid.eq(library_uuid.as_str()))
        .one(db)
        .await
    {
        Ok(Some(p)) => Some(p),
        Ok(None) => {
            if !peer_url.is_empty() {
                peer::Entity::find()
                    .filter(peer::Column::Url.eq(peer_url))
                    .one(db)
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    let peer_model = match found_peer {
        Some(p) => p,
        None => {
            return (
                StatusCode::OK,
                Json(json!({ "message": "Peer not found, already disconnected" })),
            )
                .into_response();
        }
    };

    // 5. Verify HMAC - requires key_exchange_done + x25519_public_key
    if !peer_model.key_exchange_done {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Key exchange not completed with this peer" })),
        )
            .into_response();
    }

    let peer_x25519_bytes = match &peer_model.x25519_public_key {
        Some(hex_str) => match hex::decode(hex_str) {
            Ok(b) if b.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&b);
                arr
            }
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "Invalid peer x25519 key" })),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Peer missing x25519_public_key" })),
            )
                .into_response();
        }
    };

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Crypto service not initialized" })),
            )
                .into_response();
        }
    };

    let peer_pub = x25519_dalek::PublicKey::from(peer_x25519_bytes);
    let valid = crate::crypto::key_exchange::verify_disconnect_hmac(
        crypto_service.identity().x25519_static_secret(),
        &peer_pub,
        &library_uuid,
        &timestamp,
        &hmac_bytes,
    );

    if !valid {
        tracing::warn!(
            "Disconnect HMAC verification failed for library_uuid={}",
            library_uuid
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "HMAC verification failed" })),
        )
            .into_response();
    }

    // 6. Re-handshake: confirm with the sender that they really disconnected
    let our_library_uuid = state
        .identity_service
        .library_uuid()
        .map(|s| s.to_string())
        .unwrap_or_default();

    match verify_disconnect_with_peer(&peer_model.url, &our_library_uuid).await {
        Some(false) => {
            tracing::warn!(
                "Re-handshake: peer {} denied disconnect (spoofed notification)",
                peer_model.name
            );
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "Peer denied the disconnect" })),
            )
                .into_response();
        }
        Some(true) | None => {
            // Confirmed or unreachable (timeout) - proceed with deletion
        }
    }

    // 7. Delete the peer
    let peer_name = peer_model.name.clone();
    let peer_id = peer_model.id;
    match crate::services::hub_catalog_readers::delete_peer_that_left(db, &peer_model).await {
        Ok(()) => {
            info!(
                "Peer {} ({}) removed via authenticated disconnect (uuid={})",
                peer_name, peer_id, library_uuid
            );
            revoke_hub_follow_for_peer(&state, &peer_model);
            (
                StatusCode::OK,
                Json(json!({ "message": "Disconnect acknowledged" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct DisconnectNotification {
    pub peer_url: String,
    /// Stable library UUID - used as primary lookup for peer identification.
    pub library_uuid: Option<String>,
    /// RFC3339 timestamp of the disconnect event (required for HMAC verification).
    pub timestamp: Option<String>,
    /// Hex-encoded 32-byte HMAC (required for authentication).
    pub hmac: Option<String>,
}

/// Request body for the re-handshake confirmation endpoint.
#[derive(Debug, Deserialize)]
pub struct VerifyDisconnectRequest {
    /// The library_uuid of the peer asking for confirmation.
    pub library_uuid: String,
}

/// Re-handshake endpoint: a peer asks us "did you really disconnect from me?"
///
/// Returns `confirmed: true` if we no longer have this peer in our database
/// (meaning we did initiate a disconnect). Returns `confirmed: false` if the
/// peer still exists (the disconnect was likely spoofed).
pub async fn verify_disconnect(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<VerifyDisconnectRequest>,
) -> impl IntoResponse {
    let db = state.db();
    let uuid = req.library_uuid.trim();
    if uuid.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Missing library_uuid" })),
        )
            .into_response();
    }

    // If the peer is NOT in our database, we confirm the disconnect
    let still_exists = peer::Entity::find()
        .filter(peer::Column::LibraryUuid.eq(uuid))
        .count(db)
        .await
        .unwrap_or(0)
        > 0;

    let confirmed = !still_exists;
    (StatusCode::OK, Json(json!({ "confirmed": confirmed }))).into_response()
}

/// Ask a remote peer to confirm that they really initiated a disconnect.
///
/// Returns:
/// - `Some(true)`: peer confirms (they no longer have us)
/// - `Some(false)`: peer denies (they still have us - disconnect was spoofed)
/// - `None`: peer unreachable (timeout, network error)
pub(crate) async fn verify_disconnect_with_peer(
    peer_url: &str,
    our_library_uuid: &str,
) -> Option<bool> {
    if let Err(e) = validate_url(peer_url) {
        tracing::warn!("verify_disconnect: invalid peer URL {}: {}", peer_url, e);
        return None;
    }

    let client = get_safe_client();
    let url = format!("{}/api/peers/verify-disconnect", peer_url);

    match client
        .post(&url)
        .json(&json!({ "library_uuid": our_library_uuid }))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => {
            if let Ok(body) = res.json::<serde_json::Value>().await {
                body.get("confirmed").and_then(|v| v.as_bool())
            } else {
                None
            }
        }
        Ok(res) => {
            tracing::info!(
                "verify_disconnect: peer {} returned status {}",
                peer_url,
                res.status()
            );
            None
        }
        Err(e) => {
            tracing::info!("verify_disconnect: peer {} unreachable: {}", peer_url, e);
            None
        }
    }
}
