//! Who may read this library's encrypted directory catalog (ADR-072 D3, D4,
//! D8), kept on the device.
//!
//! The hub's followers list says who follows, not who the owner trusts: a
//! paired peer that was removed stays an active follower until its own
//! device unfollows, and the X25519 key the hub returns for a follower is
//! whatever that follower's profile last published. So the owner keeps its
//! own list, `catalog_readers`, with the key pinned when the reader was let
//! in, and only that list receives wraps of the catalog key and counts in
//! the clear-or-encrypted decision.
//!
//! A reader gets in through:
//! - an approval of its follow request (manual, or the paired-peer
//!   auto-approval of ADR-053), with the key of the pairing when it is a
//!   paired peer, the key the hub returned otherwise (trust on first use);
//! - a pairing: an accepted peer holding an active follow is a reader, with
//!   the key exchanged at pairing time;
//! - the bootstrap: once, every active follower of the first readable list
//!   (trust on first use, for follows approved before this list existed);
//! - a public library (listed without approval): its active followers were
//!   let in by the owner's choice of an open directory listing.
//!
//! A reader leaves when its pairing ends or when it no longer follows. Each
//! departure is counted, and a catalog key created before the last counted
//! departure must be rotated before the next encrypted push. The count lives
//! in the database, so the rotation happens even when the departure was
//! recorded by a run that has since been closed.
//!
//! The list is local to this device, like the catalog key: each device has
//! its own node id and its own key (ADR-072 Q4), so the table is never
//! replicated by the account sync.

use std::collections::HashMap;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use super::hub_directory_service::HubFollow;

/// How a reader got onto the list. Stored as text, kept for diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderSource {
    Approval,
    Paired,
    Bootstrap,
    Public,
}

impl ReaderSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ReaderSource::Approval => "approval",
            ReaderSource::Paired => "paired",
            ReaderSource::Bootstrap => "bootstrap",
            ReaderSource::Public => "public",
        }
    }
}

/// Where one follower stands toward the encrypted catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderStatus {
    /// On the list, and the hub returns the key that was pinned.
    Reader,
    /// On the list, but the hub now returns another key (or none): the
    /// follower reinstalled, or the key was substituted. No longer an
    /// authorised reader: no wrap goes out and D8 does not count it, so the
    /// catalog stays encrypted for the others. Not a departure either (the
    /// owner withdrew no trust, nothing to rotate): the follower is a reader
    /// again once the hub returns the pinned key, or once the owner pins
    /// the new one by approving a new request or pairing it again.
    KeyMismatch,
    /// Taken off the list by the owner (an ended pairing): no wrap, and
    /// not counted by D8, so an ex-peer still following holds no one back.
    NotReader,
    /// Never on the list (approved without a key to pin, followed while
    /// the library was public and not seen then, a failed admission, or a
    /// follow the hub alone vouches for): no wrap, and D8 counts it as
    /// unable to read, so the catalog stays in clear rather than locking
    /// out someone the owner may have let in.
    Unknown,
}

impl ReaderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ReaderStatus::Reader => "reader",
            ReaderStatus::KeyMismatch => "key_mismatch",
            ReaderStatus::NotReader => "not_reader",
            ReaderStatus::Unknown => "unknown",
        }
    }
}

/// Why a reader left the list. A departure the hub reported can be undone
/// by the hub showing the follow again with the pinned key; an ended pairing
/// is the owner's decision and only a new pairing the owner accepted lifts it.
const REASON_FOLLOWER_LEFT: &str = "follower left";
const REASON_PAIRING_ENDED: &str = "pairing ended";
/// The other library ended the pairing: off the list, nothing to restore.
const REASON_PEER_LEFT: &str = "peer left";
/// An ended pairing the owner restored (accepted the library again, or
/// scanned its invitation): the next reconciliation may let it back in.
const REASON_PAIRING_RESTORED: &str = "pairing restored";

/// One row of the list, current or revoked.
#[derive(Debug, Clone)]
struct ReaderRow {
    key: String,
    source: String,
    revoked_reason: Option<String>,
    revoked: bool,
}

/// Snapshot of the list: current readers with their pinned key, and the
/// readers taken off it.
#[derive(Debug, Default, Clone)]
pub struct CatalogReaders {
    rows: HashMap<String, ReaderRow>,
}

impl CatalogReaders {
    /// Reads every row of the list.
    pub async fn load(db: &DatabaseConnection) -> Result<Self, String> {
        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT node_id, x25519_public_key, source, revoked_at, revoke_reason
                 FROM catalog_readers"
                    .to_owned(),
            ))
            .await
            .map_err(|e| format!("catalog readers: {e}"))?;
        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            let get = |col: &str| -> Result<Option<String>, String> {
                row.try_get::<Option<String>>("", col)
                    .map_err(|e| format!("catalog readers: {e}"))
            };
            let node_id = get("node_id")?.unwrap_or_default();
            map.insert(
                node_id,
                ReaderRow {
                    key: get("x25519_public_key")?.unwrap_or_default(),
                    source: get("source")?.unwrap_or_default(),
                    revoked: get("revoked_at")?.is_some(),
                    revoked_reason: get("revoke_reason")?,
                },
            );
        }
        Ok(Self { rows: map })
    }

    /// The key pinned for `node_id`, when it is a current reader.
    pub fn pinned_key(&self, node_id: &str) -> Option<&str> {
        self.rows
            .get(node_id)
            .filter(|r| !r.revoked)
            .map(|r| r.key.as_str())
    }

    /// Where `follow`'s follower stands, from its node id and the key the
    /// hub returned for it.
    pub fn status(&self, follow: &HubFollow) -> ReaderStatus {
        let Some(row) = self.rows.get(&follow.follower_node_id) else {
            return ReaderStatus::Unknown;
        };
        if row.revoked {
            // Seen leaving, then following again under another key (or
            // not yet let back in): someone the owner let in, who cannot
            // be wrapped for. An ended pairing keeps it out.
            return if row.revoked_reason.as_deref() == Some(REASON_FOLLOWER_LEFT) {
                ReaderStatus::KeyMismatch
            } else {
                ReaderStatus::NotReader
            };
        }
        match follow
            .follower_x25519_public_key
            .as_deref()
            .and_then(normalized_key)
        {
            Some(key) if key == row.key => ReaderStatus::Reader,
            _ => ReaderStatus::KeyMismatch,
        }
    }

    fn has_row(&self, node_id: &str) -> bool {
        self.rows.contains_key(node_id)
    }
}

/// Lower-case hex of a 32-byte X25519 key, or None when `hex_key` is not
/// one. Every key is stored and compared in this form.
fn normalized_key(hex_key: &str) -> Option<String> {
    let bytes = hex::decode(hex_key.trim()).ok()?;
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    Some(hex::encode(bytes))
}

/// A library uuid as the hub writes node ids (lower case, no spaces), or
/// None when blank. Uuids from peers rows and requests go through it.
fn node_key(uuid: &str) -> Option<String> {
    Some(uuid.trim().to_ascii_lowercase()).filter(|u| !u.is_empty())
}

/// A signing key as the mark stores and compares it: hex is decoded in
/// either case, so the case must not tell two keys apart.
fn signing_key(ed25519: &str) -> Option<String> {
    Some(ed25519.trim().to_ascii_lowercase()).filter(|k| !k.is_empty())
}

/// Puts `node_id` on the list with `x25519_hex` pinned, or refreshes it:
/// a revoked reader comes back, a current one gets the new key and source.
/// Counts the admission, so the next push hands out the wraps even when
/// the catalog did not change. Returns false, writing nothing, when the
/// key is not 32 bytes of hex, and when the owner ended the pairing with
/// `node_id` and did not restore it: an approval admitted before the
/// removal may land after it.
pub async fn enrol(
    db: &DatabaseConnection,
    node_id: &str,
    x25519_hex: &str,
    source: ReaderSource,
) -> Result<bool, String> {
    let Some(key) = normalized_key(x25519_hex) else {
        return Ok(false);
    };
    if !upsert_reader(db, node_id, &key, source.as_str()).await? {
        tracing::warn!(
            target: "hub_directory",
            "catalog reader {} not let in, its pairing was ended",
            short(node_id)
        );
        return Ok(false);
    }
    tracing::info!(
        target: "hub_directory",
        "catalog reader {} let in ({})",
        short(node_id),
        source.as_str()
    );
    Ok(true)
}

/// Writes the row unless it carries an ended pairing; returns whether it
/// wrote.
async fn upsert_reader(
    db: &DatabaseConnection,
    node_id: &str,
    key: &str,
    source: &str,
) -> Result<bool, String> {
    use sea_orm::TransactionTrait;

    let txn = db
        .begin()
        .await
        .map_err(|e| format!("catalog reader enrolment: {e}"))?;
    let written = txn
        .execute(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "INSERT INTO catalog_readers
                 (node_id, x25519_public_key, source, approved_at, revoked_at, revoke_reason)
             VALUES (?, ?, ?, ?, NULL, NULL)
             ON CONFLICT(node_id) DO UPDATE SET
                 x25519_public_key = excluded.x25519_public_key,
                 source = excluded.source,
                 approved_at = excluded.approved_at,
                 revoked_at = NULL,
                 revoke_reason = NULL
             WHERE catalog_readers.revoke_reason IS NOT ?",
            [
                node_id.into(),
                key.into(),
                source.into(),
                now_rfc3339().into(),
                REASON_PAIRING_ENDED.into(),
            ],
        ))
        .await
        .map_err(|e| format!("catalog reader enrolment: {e}"))?
        .rows_affected()
        > 0;
    if !written {
        return Ok(false);
    }
    ensure_state(&txn).await?;
    txn.execute(Statement::from_string(
        txn.get_database_backend(),
        "UPDATE catalog_reader_state SET enrolments = enrolments + 1 WHERE id = 1".to_owned(),
    ))
    .await
    .map_err(|e| format!("catalog reader enrolment: {e}"))?;
    txn.commit()
        .await
        .map_err(|e| format!("catalog reader enrolment: {e}"))?;
    Ok(true)
}

/// Whether a follow approval for `follower_node_id` may go to the hub.
/// The paired-peer auto-approval (ADR-053) is refused for a library whose
/// pairing the owner ended and did not restore, before the hub hears of it:
/// an active follow lets it read a catalog in clear and carries the contact
/// card. The owner's own approval is a gesture and restores the pairing.
pub async fn admit_approval(
    db: &DatabaseConnection,
    follower_node_id: Option<&str>,
    owner_gesture: bool,
) -> Result<(), String> {
    if owner_gesture {
        return restore_pairing(db, follower_node_id, None).await;
    }
    let Some(node_id) = follower_node_id else {
        return Err("automatic approval without a follower node id".to_owned());
    };
    if pairing_ended(db, Some(node_id), None).await? {
        tracing::warn!(
            target: "hub_directory",
            "automatic approval of {} refused, its pairing was ended",
            short(node_id)
        );
        return Err("the pairing with this library was ended".to_owned());
    }
    Ok(())
}

/// The key to pin and wrap for when the owner approves `follower_node_id`:
/// the pairing key when the follower is an accepted paired peer, the key
/// the hub returned otherwise. None when neither exists, and for a paired
/// peer whose pairing the owner ended and never restored: that row was not
/// the owner's doing, and its follow is approved by the paired-peer
/// auto-approval (ADR-053), not by the owner.
pub async fn key_for_approval(
    db: &DatabaseConnection,
    follower_node_id: &str,
    hub_key: Option<&str>,
) -> Result<Option<(String, ReaderSource)>, String> {
    if let Some(key) = paired_keys(db).await?.remove(follower_node_id) {
        if pairing_ended(db, Some(follower_node_id), None).await? {
            tracing::warn!(
                target: "hub_directory",
                "follow from {} approved as a paired peer whose pairing was ended, not let in",
                short(follower_node_id)
            );
            return Ok(None);
        }
        return Ok(Some((key, ReaderSource::Paired)));
    }
    Ok(hub_key
        .and_then(normalized_key)
        .map(|key| (key, ReaderSource::Approval)))
}

/// Takes `node_id` off the list and counts the departure, so the next
/// encrypted push rotates the catalog key. Returns whether a current reader
/// was revoked; revoking someone who is not on the list counts nothing.
pub async fn revoke<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    node_id: &str,
    reason: &str,
) -> Result<bool, String> {
    let txn = db
        .begin()
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?;
    let revoked = revoke_current(&txn, node_id, reason).await?;
    if revoked {
        count_departure(&txn).await?;
    }
    txn.commit()
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?;
    if revoked {
        tracing::info!(
            target: "hub_directory",
            "catalog reader {} revoked ({reason}), key rotation due",
            short(node_id)
        );
    }
    Ok(revoked)
}

async fn revoke_current<C: ConnectionTrait>(
    conn: &C,
    node_id: &str,
    reason: &str,
) -> Result<bool, String> {
    Ok(conn
        .execute(Statement::from_sql_and_values(
            conn.get_database_backend(),
            "UPDATE catalog_readers SET revoked_at = ?, revoke_reason = ?
             WHERE node_id = ? AND revoked_at IS NULL",
            [now_rfc3339().into(), reason.into(), node_id.into()],
        ))
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?
        .rows_affected()
        > 0)
}

async fn count_departure<C: ConnectionTrait>(conn: &C) -> Result<(), String> {
    ensure_state(conn).await?;
    conn.execute(Statement::from_string(
        conn.get_database_backend(),
        "UPDATE catalog_reader_state SET revocations = revocations + 1 WHERE id = 1".to_owned(),
    ))
    .await
    .map_err(|e| format!("catalog reader revocation: {e}"))?;
    Ok(())
}

/// Records the end of a pairing with the library `library_uuid`, signing
/// with `ed25519`. Callers run it in the transaction that deletes the peer
/// row, so the two land together: deleted, so a reconciliation can no
/// longer read the pairing and let it back in; recorded, so the next
/// encrypted push rotates the key even if the app closes right after.
///
/// The mark holds whether or not the library was on the list: a pairing
/// ended before the first followers list could be read must survive the
/// bootstrap, and a wrap handed out before the list existed may still be
/// out there, so the departure is counted. A reader that had already been
/// seen leaving now stays out for good. The signing key is kept with the
/// mark, so a request that leaves out the uuid is still recognised. Only
/// [`restore_pairing`] lifts it: no admission does (see [`enrol`]).
///
/// A peer known by its signing key alone gets a mark on that key and no
/// departure: the list is keyed by uuid, so no pairing of it ever made a
/// reader (one let in by a follow approval stays, that was the owner's own
/// call).
///
/// Returns whether a departure was counted. No-op with neither key.
pub async fn record_pairing_end<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    library_uuid: Option<&str>,
    ed25519: Option<&str>,
) -> Result<bool, String> {
    let ed25519 = ed25519.and_then(signing_key);
    let Some(uuid) = library_uuid.and_then(node_key) else {
        if let Some(ed) = ed25519 {
            mark_signing_key(db, &ed).await?;
        }
        return Ok(false);
    };
    let txn = db
        .begin()
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?;
    let now = now_rfc3339();
    let counted = if revoke_current(&txn, &uuid, REASON_PAIRING_ENDED).await? {
        true
    } else {
        // Already off the list: make the mark final, dated now so that no
        // list read before it can let the library back. Never on it: leave
        // a mark with no key, which nothing can wrap for.
        txn.execute(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "UPDATE catalog_readers SET revoke_reason = ?, revoked_at = ? WHERE node_id = ?",
            [
                REASON_PAIRING_ENDED.into(),
                now.clone().into(),
                uuid.clone().into(),
            ],
        ))
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?;
        txn.execute(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "INSERT INTO catalog_readers
                 (node_id, x25519_public_key, source, approved_at, revoked_at, revoke_reason)
             VALUES (?, '', ?, ?, ?, ?)
             ON CONFLICT(node_id) DO NOTHING",
            [
                uuid.clone().into(),
                ReaderSource::Paired.as_str().into(),
                now.clone().into(),
                now.into(),
                REASON_PAIRING_ENDED.into(),
            ],
        ))
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?
        .rows_affected()
            > 0
    };
    if let Some(ed) = ed25519 {
        txn.execute(Statement::from_sql_and_values(
            txn.get_database_backend(),
            "UPDATE catalog_readers SET ed25519_public_key = ? WHERE node_id = ?",
            [ed.into(), uuid.clone().into()],
        ))
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?;
    }
    if counted {
        count_departure(&txn).await?;
    }
    txn.commit()
        .await
        .map_err(|e| format!("catalog reader revocation: {e}"))?;
    tracing::info!(
        target: "hub_directory",
        "pairing with {} ended, catalog key rotation {}",
        short(&uuid),
        if counted { "due" } else { "already due or done" }
    );
    Ok(counted)
}

/// The mark of an ended pairing with a library known by its signing key
/// alone, under a node id no hub ever issues.
async fn mark_signing_key<C: ConnectionTrait>(conn: &C, ed25519: &str) -> Result<(), String> {
    let now = now_rfc3339();
    conn.execute(Statement::from_sql_and_values(
        conn.get_database_backend(),
        "INSERT INTO catalog_readers
             (node_id, x25519_public_key, source, approved_at, revoked_at, revoke_reason,
              ed25519_public_key)
         VALUES (?, '', ?, ?, ?, ?, ?)
         ON CONFLICT(node_id) DO UPDATE SET
             revoked_at = excluded.revoked_at,
             revoke_reason = excluded.revoke_reason",
        [
            format!("ed:{ed25519}").into(),
            ReaderSource::Paired.as_str().into(),
            now.clone().into(),
            now.into(),
            REASON_PAIRING_ENDED.into(),
            ed25519.into(),
        ],
    ))
    .await
    .map_err(|e| format!("catalog reader revocation: {e}"))?;
    tracing::info!(target: "hub_directory", "pairing with a library without uuid ended");
    Ok(())
}

/// Deletes the row of a peer that ended the pairing itself, and takes it
/// off the readers list in the same transaction. No mark: this owner
/// decided nothing, and the peer may pair again as any library would. The
/// departure is counted, since it held a wrap of the current key.
pub async fn delete_peer_that_left(
    db: &DatabaseConnection,
    peer_model: &crate::models::peer::Model,
) -> Result<(), String> {
    use sea_orm::{EntityTrait, TransactionTrait};

    let txn = db.begin().await.map_err(|e| e.to_string())?;
    crate::models::peer::Entity::delete_by_id(peer_model.id)
        .exec(&txn)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(uuid) = peer_model.library_uuid.as_deref().and_then(node_key) {
        revoke(&txn, &uuid, REASON_PEER_LEFT).await?;
    }
    txn.commit().await.map_err(|e| e.to_string())
}

/// Deletes a peer row and records the end of its pairing in one
/// transaction: the ex-peer can never be gone from `peers` while still a
/// catalog reader, even if the app is killed between the two, and a failed
/// record leaves the pairing in place. Every path that ends a pairing goes
/// through here.
pub async fn delete_peer_ending_pairing(
    db: &DatabaseConnection,
    peer_model: &crate::models::peer::Model,
) -> Result<(), String> {
    use sea_orm::{EntityTrait, TransactionTrait};

    let txn = db.begin().await.map_err(|e| e.to_string())?;
    crate::models::peer::Entity::delete_by_id(peer_model.id)
        .exec(&txn)
        .await
        .map_err(|e| e.to_string())?;
    record_pairing_end(
        &txn,
        peer_model.library_uuid.as_deref(),
        peer_model.public_key.as_deref(),
    )
    .await?;
    txn.commit().await.map_err(|e| e.to_string())
}

/// Writes `peer_model` and, when `restore` names a library (its uuid and
/// signing key), lifts the mark of its ended pairing in the same
/// transaction: the owner's acceptance and the restoration land together
/// or not at all.
pub async fn update_peer_restoring_pairing(
    db: &DatabaseConnection,
    peer_model: crate::models::peer::ActiveModel,
    restore: Option<(Option<String>, Option<String>)>,
) -> Result<crate::models::peer::Model, String> {
    use sea_orm::{ActiveModelTrait, TransactionTrait};

    let txn = db.begin().await.map_err(|e| e.to_string())?;
    if let Some((uuid, ed25519)) = &restore {
        restore_pairing(&txn, uuid.as_deref(), ed25519.as_deref()).await?;
    }
    let updated = peer_model.update(&txn).await.map_err(|e| e.to_string())?;
    txn.commit().await.map_err(|e| e.to_string())?;
    Ok(updated)
}

/// Whether this library ended the pairing with the library `library_uuid`,
/// or signing with `ed25519`, and the owner has not restored it since: a
/// request from it waits for the owner, and its messages are refused.
pub async fn pairing_ended<C: ConnectionTrait>(
    db: &C,
    library_uuid: Option<&str>,
    ed25519: Option<&str>,
) -> Result<bool, String> {
    let uuid = library_uuid.and_then(node_key);
    let ed = ed25519.and_then(signing_key);
    if uuid.is_none() && ed.is_none() {
        return Ok(false);
    }
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT 1 AS ended FROM catalog_readers
             WHERE revoked_at IS NOT NULL AND revoke_reason = ?
               AND (node_id = ? OR ed25519_public_key = ?)",
            [REASON_PAIRING_ENDED.into(), uuid.into(), ed.into()],
        ))
        .await
        .map_err(|e| format!("catalog readers: {e}"))?;
    Ok(row.is_some())
}

/// Lifts the mark of an ended pairing, on the owner's own gesture
/// (accepting the library again, scanning its invitation). The library is
/// not a reader yet: the next reconciliation lets it back in when it is an
/// accepted peer with an active follow.
pub async fn restore_pairing<C: ConnectionTrait>(
    db: &C,
    library_uuid: Option<&str>,
    ed25519: Option<&str>,
) -> Result<(), String> {
    let uuid = library_uuid.and_then(node_key);
    let ed = ed25519.and_then(signing_key);
    if uuid.is_none() && ed.is_none() {
        return Ok(());
    }
    let restored = db
        .execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "UPDATE catalog_readers SET revoke_reason = ?
             WHERE revoked_at IS NOT NULL AND revoke_reason = ?
               AND (node_id = ? OR ed25519_public_key = ?)",
            [
                REASON_PAIRING_RESTORED.into(),
                REASON_PAIRING_ENDED.into(),
                uuid.into(),
                ed.into(),
            ],
        ))
        .await
        .map_err(|e| format!("catalog readers: {e}"))?
        .rows_affected();
    if restored > 0 {
        tracing::info!(target: "hub_directory", "ended pairing restored by the owner");
    }
    Ok(())
}

/// Whether the library `library_uuid` is a current catalog reader: it holds
/// a wrap of the current key, so its row may only leave `peers` through a
/// path that also takes it off the list. False without a uuid.
pub async fn is_current_reader<C: ConnectionTrait>(
    db: &C,
    library_uuid: Option<&str>,
) -> Result<bool, String> {
    let Some(uuid) = library_uuid.and_then(node_key) else {
        return Ok(false);
    };
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT 1 AS current FROM catalog_readers WHERE node_id = ? AND revoked_at IS NULL",
            [uuid.into()],
        ))
        .await
        .map_err(|e| format!("catalog readers: {e}"))?;
    Ok(row.is_some())
}

/// Whether messages from `peer` must be refused: the owner ended the
/// pairing with it and did not restore it. A peer accepted again by the
/// owner had its mark lifted, so a mark still standing on an accepted row
/// means the row was not the owner's doing.
pub async fn held_back<C: ConnectionTrait>(
    db: &C,
    peer: &crate::models::peer::Model,
) -> Result<bool, String> {
    pairing_ended(db, peer.library_uuid.as_deref(), peer.public_key.as_deref()).await
}

/// Brings the list in step with a followers list read from the hub at
/// `listed_at` (RFC 3339, taken before the request went out).
///
/// 1. Bootstrap, once: every active follower without a row.
/// 2. Paired peers: a library taken off the list comes back with the key
///    of its current pairing, unless its pairing ended and the owner did
///    not restore it. A pairing never adds someone who was never on the
///    list (the peers row may come from a deposit nobody checked), and
///    never moves an existing pin.
/// 3. A public library: active followers without a row come in.
/// 4. A reader seen leaving comes back when the hub shows it following
///    again with the pinned key.
/// 5. Departures: a current reader the list no longer shows as following.
/// 6. Missing wraps: a reader the hub shows with its pinned key but no wrap
///    (withdrawn while the hub returned another key, or never delivered)
///    makes a wrap batch due, so the next push hands it out even when the
///    catalog did not change.
///
/// A reader let in at or after `listed_at` is never taken off on the
/// strength of a list that predates it, and a reader revoked at or after
/// `listed_at` is never let back in by it.
pub async fn reconcile(
    db: &DatabaseConnection,
    followers: &[HubFollow],
    listed_at: &str,
) -> Result<(), String> {
    ensure_state(db).await?;
    let active: Vec<&HubFollow> = followers.iter().filter(|f| f.status == "active").collect();
    let paired = paired_keys(db).await?;
    let key_of = |f: &HubFollow| -> Option<String> {
        paired.get(&f.follower_node_id).cloned().or_else(|| {
            f.follower_x25519_public_key
                .as_deref()
                .and_then(normalized_key)
        })
    };

    // 1. Bootstrap.
    if !is_seeded(db).await? {
        let current = CatalogReaders::load(db).await?;
        for f in &active {
            if current.has_row(&f.follower_node_id) {
                continue;
            }
            if let Some(key) = key_of(f) {
                enrol(db, &f.follower_node_id, &key, ReaderSource::Bootstrap).await?;
            }
        }
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "UPDATE catalog_reader_state SET seeded = 1 WHERE id = 1".to_owned(),
        ))
        .await
        .map_err(|e| format!("catalog readers bootstrap: {e}"))?;
    }

    // 2. Paired peers.
    let current = CatalogReaders::load(db).await?;
    for f in &active {
        let Some(key) = paired.get(&f.follower_node_id) else {
            continue;
        };
        let Some(row) = current.rows.get(&f.follower_node_id) else {
            continue;
        };
        // An ended pairing comes back only once the owner restored it: a
        // peers row alone may be the ex-peer pairing itself back in.
        if !row.revoked
            || row.revoked_reason.as_deref() == Some(REASON_PAIRING_ENDED)
            || revoked_since(db, &f.follower_node_id, listed_at).await?
        {
            continue;
        }
        enrol(db, &f.follower_node_id, key, ReaderSource::Paired).await?;
    }

    // 3. A public library.
    if is_public(db).await? {
        let current = CatalogReaders::load(db).await?;
        for f in &active {
            if current.has_row(&f.follower_node_id) {
                continue;
            }
            if let Some(key) = key_of(f) {
                enrol(db, &f.follower_node_id, &key, ReaderSource::Public).await?;
            }
        }
    }

    // 4. Back after a departure the hub reported.
    let current = CatalogReaders::load(db).await?;
    for f in &active {
        let Some(row) = current.rows.get(&f.follower_node_id) else {
            continue;
        };
        if !row.revoked || row.revoked_reason.as_deref() != Some(REASON_FOLLOWER_LEFT) {
            continue;
        }
        let same_key = f
            .follower_x25519_public_key
            .as_deref()
            .and_then(normalized_key)
            .is_some_and(|k| k == row.key);
        if !same_key || revoked_since(db, &f.follower_node_id, listed_at).await? {
            continue;
        }
        let source = row.source.clone();
        if !upsert_reader(db, &f.follower_node_id, &row.key, &source).await? {
            continue;
        }
        tracing::info!(
            target: "hub_directory",
            "catalog reader {} follows again with its pinned key, let back in",
            short(&f.follower_node_id)
        );
    }

    // 5. Departures.
    let following: std::collections::HashSet<&str> =
        active.iter().map(|f| f.follower_node_id.as_str()).collect();
    let rows = db
        .query_all(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT node_id FROM catalog_readers WHERE revoked_at IS NULL AND approved_at < ?",
            [listed_at.into()],
        ))
        .await
        .map_err(|e| format!("catalog readers: {e}"))?;
    for row in rows {
        let node_id: String = row
            .try_get("", "node_id")
            .map_err(|e| format!("catalog readers: {e}"))?;
        if !following.contains(node_id.as_str()) {
            revoke(db, &node_id, REASON_FOLLOWER_LEFT).await?;
        }
    }

    // 6. Missing wraps.
    let current = CatalogReaders::load(db).await?;
    let unwrapped = active.iter().any(|f| {
        current.status(f) == ReaderStatus::Reader
            && f.encrypted_catalog_key.as_deref().is_none_or(str::is_empty)
    });
    if unwrapped {
        ask_for_wraps(db).await?;
    }
    Ok(())
}

/// Makes a wrap batch due, unless one already is: counts as an admission
/// for [`wraps_due`] without letting anyone in.
async fn ask_for_wraps(db: &DatabaseConnection) -> Result<(), String> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "UPDATE catalog_reader_state SET enrolments = enrolments + 1
         WHERE id = 1 AND enrolments <= wrapped_through"
            .to_owned(),
    ))
    .await
    .map_err(|e| format!("catalog reader state: {e}"))?;
    Ok(())
}

/// The departure count a rotation would cover, when the current catalog
/// key predates the last departure; None when no rotation is due.
pub async fn rotation_due(db: &DatabaseConnection) -> Result<Option<i64>, String> {
    let Some((revocations, rotated)) = state_pair(db, "revocations", "rotated_through").await?
    else {
        return Ok(None);
    };
    Ok((revocations > rotated).then_some(revocations))
}

/// Records that the catalog key now in use was created after the first
/// `through` departures. Never moves backwards.
pub async fn mark_rotated(db: &DatabaseConnection, through: i64) -> Result<(), String> {
    advance_state(db, "rotated_through", through).await
}

/// The admission count a wrap batch would cover, when a reader came in
/// since the last batch the hub accepted; None otherwise.
pub async fn wraps_due(db: &DatabaseConnection) -> Result<Option<i64>, String> {
    let Some((enrolments, wrapped)) = state_pair(db, "enrolments", "wrapped_through").await? else {
        return Ok(None);
    };
    Ok((enrolments > wrapped).then_some(enrolments))
}

/// Records that the readers admitted through `through` hold their wrap.
/// Never moves backwards.
pub async fn mark_wrapped(db: &DatabaseConnection, through: i64) -> Result<(), String> {
    advance_state(db, "wrapped_through", through).await
}

/// Two counters of the state row. Column names are compile-time constants
/// of this module, never caller input.
async fn state_pair(
    db: &DatabaseConnection,
    count: &'static str,
    done: &'static str,
) -> Result<Option<(i64, i64)>, String> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT {count} AS n, {done} AS d FROM catalog_reader_state WHERE id = 1"),
        ))
        .await
        .map_err(|e| format!("catalog reader state: {e}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let n: i64 = row
        .try_get("", "n")
        .map_err(|e| format!("catalog reader state: {e}"))?;
    let d: i64 = row
        .try_get("", "d")
        .map_err(|e| format!("catalog reader state: {e}"))?;
    Ok(Some((n, d)))
}

async fn advance_state(
    db: &DatabaseConnection,
    column: &'static str,
    through: i64,
) -> Result<(), String> {
    ensure_state(db).await?;
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        format!("UPDATE catalog_reader_state SET {column} = MAX({column}, ?) WHERE id = 1"),
        [through.into()],
    ))
    .await
    .map_err(|e| format!("catalog reader state: {e}"))?;
    Ok(())
}

/// Empties the list and its state, for a full app reset.
pub async fn clear(db: &DatabaseConnection) -> Result<(), String> {
    for sql in [
        "DELETE FROM catalog_readers",
        "DELETE FROM catalog_reader_state",
    ] {
        db.execute(Statement::from_string(
            db.get_database_backend(),
            sql.to_owned(),
        ))
        .await
        .map_err(|e| format!("catalog readers reset: {e}"))?;
    }
    Ok(())
}

async fn ensure_state<C: ConnectionTrait>(conn: &C) -> Result<(), String> {
    conn.execute(Statement::from_string(
        conn.get_database_backend(),
        "INSERT OR IGNORE INTO catalog_reader_state (id) VALUES (1)".to_owned(),
    ))
    .await
    .map_err(|e| format!("catalog reader state: {e}"))?;
    Ok(())
}

async fn is_seeded(db: &DatabaseConnection) -> Result<bool, String> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT seeded FROM catalog_reader_state WHERE id = 1".to_owned(),
        ))
        .await
        .map_err(|e| format!("catalog reader state: {e}"))?;
    Ok(row
        .and_then(|r| r.try_get::<i64>("", "seeded").ok())
        .is_some_and(|v| v != 0))
}

/// Whether `node_id` was revoked at or after `listed_at`.
async fn revoked_since(
    db: &DatabaseConnection,
    node_id: &str,
    listed_at: &str,
) -> Result<bool, String> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT revoked_at FROM catalog_readers WHERE node_id = ?",
            [node_id.into()],
        ))
        .await
        .map_err(|e| format!("catalog readers: {e}"))?;
    Ok(row
        .and_then(|r| r.try_get::<Option<String>>("", "revoked_at").ok())
        .flatten()
        .is_some_and(|at| at.as_str() >= listed_at))
}

/// Pairing keys of the accepted peers, by library uuid: the X25519 key
/// exchanged at pairing time, a stronger source than the hub profile.
async fn paired_keys(db: &DatabaseConnection) -> Result<HashMap<String, String>, String> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            "SELECT library_uuid, x25519_public_key FROM peers
             WHERE connection_status = 'accepted'
               AND library_uuid IS NOT NULL AND x25519_public_key IS NOT NULL"
                .to_owned(),
        ))
        .await
        .map_err(|e| format!("paired peers: {e}"))?;
    let mut keys = HashMap::with_capacity(rows.len());
    for row in rows {
        let uuid: String = row
            .try_get("", "library_uuid")
            .map_err(|e| format!("paired peers: {e}"))?;
        let key: String = row
            .try_get("", "x25519_public_key")
            .map_err(|e| format!("paired peers: {e}"))?;
        if let (Some(uuid), Some(key)) = (node_key(&uuid), normalized_key(&key)) {
            keys.insert(uuid, key);
        }
    }
    Ok(keys)
}

/// Whether the library is listed without approval (ADR-072 D1, clear).
async fn is_public(db: &DatabaseConnection) -> Result<bool, String> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT is_listed, requires_approval FROM hub_directory_config WHERE id = 1".to_owned(),
        ))
        .await
        .map_err(|e| format!("directory config: {e}"))?;
    Ok(row.is_some_and(|r| {
        let listed = r.try_get::<bool>("", "is_listed").unwrap_or(false);
        let approval = r.try_get::<bool>("", "requires_approval").unwrap_or(true);
        listed && !approval
    }))
}

/// First characters of a node id, enough to tell readers apart in a log.
pub fn short(node_id: &str) -> &str {
    node_id.get(..8).unwrap_or(node_id)
}

/// RFC 3339 with a fixed sub-second precision, so that two instants written
/// by this module compare correctly as text.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    use super::*;
    use crate::db;
    use crate::models::peer;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    fn key(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    async fn test_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn set_directory(db: &DatabaseConnection, is_listed: bool, requires_approval: bool) {
        db.execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT OR REPLACE INTO hub_directory_config
                (id, node_id, write_token, is_listed, requires_approval, created_at, updated_at)
             VALUES (1, 'owner-node', 'owner-token', ?, ?, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [is_listed.into(), requires_approval.into()],
        ))
        .await
        .unwrap();
    }

    async fn pair(db: &DatabaseConnection, uuid: &str, x25519: &str, status: &str) -> peer::Model {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set(format!("peer {uuid}")),
            url: Set(format!("http://{uuid}.local:8000")),
            library_uuid: Set(Some(uuid.to_string())),
            x25519_public_key: Set(Some(x25519.to_string())),
            key_exchange_done: Set(true),
            auto_approve: Set(status == "accepted"),
            connection_status: Set(status.to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    fn follower(id: i64, node: &str, status: &str, x25519: Option<&str>) -> HubFollow {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "follower_node_id": node,
            "followed_node_id": "owner-node",
            "status": status,
            "created_at": "2026-01-01T00:00:00Z",
            "follower_x25519_public_key": x25519,
        }))
        .unwrap()
    }

    async fn readers(db: &DatabaseConnection) -> CatalogReaders {
        CatalogReaders::load(db).await.unwrap()
    }

    // ── enrolment and status ─────────────────────────────────────────────

    #[tokio::test]
    async fn an_enrolled_reader_with_the_pinned_key_is_a_reader() {
        let db = test_db().await;
        assert!(
            enrol(&db, A, &key(1), ReaderSource::Approval)
                .await
                .unwrap()
        );

        let list = readers(&db).await;

        assert_eq!(list.pinned_key(A), Some(key(1).as_str()));
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::Reader
        );
        // Hex case is not a different key.
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1).to_uppercase()))),
            ReaderStatus::Reader
        );
    }

    #[tokio::test]
    async fn another_key_from_the_hub_is_a_mismatch_not_a_reader() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        let list = readers(&db).await;

        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(2)))),
            ReaderStatus::KeyMismatch
        );
        assert_eq!(
            list.status(&follower(1, A, "active", None)),
            ReaderStatus::KeyMismatch
        );
    }

    #[tokio::test]
    async fn a_follower_never_let_in_is_unknown() {
        // Not a reader, but not an ex-reader either: the decision keeps the
        // catalog in clear for it rather than locking it out.
        let db = test_db().await;
        let list = readers(&db).await;

        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::Unknown
        );
    }

    #[tokio::test]
    async fn a_keyless_approved_follower_is_unknown_not_shut_out() {
        // Approved before its profile carried a key: nothing to pin, so it
        // never gets on the list, and must not be ignored by the decision.
        let db = test_db().await;
        set_directory(&db, false, true).await;

        reconcile(&db, &[follower(1, A, "active", None)], &now_rfc3339())
            .await
            .unwrap();

        assert_eq!(
            readers(&db).await.status(&follower(1, A, "active", None)),
            ReaderStatus::Unknown
        );
    }

    #[tokio::test]
    async fn a_malformed_key_is_never_pinned() {
        let db = test_db().await;

        assert!(
            !enrol(&db, A, "not-hex", ReaderSource::Approval)
                .await
                .unwrap()
        );
        assert!(
            !enrol(&db, A, &"ab".repeat(31), ReaderSource::Approval)
                .await
                .unwrap()
        );

        assert_eq!(readers(&db).await.pinned_key(A), None);
    }

    #[tokio::test]
    async fn approving_a_paired_peer_pins_the_pairing_key_not_the_hub_key() {
        let db = test_db().await;
        pair(&db, A, &key(1), "accepted").await;

        let chosen = key_for_approval(&db, A, Some(&key(9))).await.unwrap();

        assert_eq!(chosen, Some((key(1), ReaderSource::Paired)));
    }

    #[tokio::test]
    async fn approving_a_stranger_pins_the_hub_key() {
        let db = test_db().await;
        // A pending pairing is not a source of trust.
        pair(&db, B, &key(2), "pending").await;

        assert_eq!(
            key_for_approval(&db, A, Some(&key(9))).await.unwrap(),
            Some((key(9), ReaderSource::Approval))
        );
        assert_eq!(
            key_for_approval(&db, B, Some(&key(9))).await.unwrap(),
            Some((key(9), ReaderSource::Approval))
        );
        assert_eq!(key_for_approval(&db, C, None).await.unwrap(), None);
    }

    // ── revocation and rotation ──────────────────────────────────────────

    #[tokio::test]
    async fn a_revoked_reader_is_off_the_list_and_a_rotation_is_due() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        assert_eq!(rotation_due(&db).await.unwrap(), None);

        assert!(revoke(&db, A, "pairing ended").await.unwrap());

        assert_eq!(readers(&db).await.pinned_key(A), None);
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn revoking_someone_off_the_list_counts_nothing() {
        let db = test_db().await;

        assert!(!revoke(&db, A, "pairing ended").await.unwrap());

        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_rotation_stays_due_across_restarts_until_marked() {
        // The departure is in the database: a new run (here, a fresh read
        // with no in-memory state) still sees it.
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        revoke(&db, A, "pairing ended").await.unwrap();

        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
        mark_rotated(&db, 1).await.unwrap();
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_departure_during_a_rotation_keeps_the_next_one_due() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        enrol(&db, B, &key(2), ReaderSource::Paired).await.unwrap();
        revoke(&db, A, "pairing ended").await.unwrap();
        let through = rotation_due(&db).await.unwrap().unwrap();

        // B leaves while the rotation covering A is under way.
        revoke(&db, B, "pairing ended").await.unwrap();
        mark_rotated(&db, through).await.unwrap();

        assert_eq!(rotation_due(&db).await.unwrap(), Some(2));
        // Marking never moves backwards.
        mark_rotated(&db, 2).await.unwrap();
        mark_rotated(&db, 1).await.unwrap();
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_re_approval_brings_a_revoked_reader_back() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        revoke(&db, A, "follower left").await.unwrap();

        enrol(&db, A, &key(3), ReaderSource::Approval)
            .await
            .unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), Some(key(3).as_str()));
    }

    #[tokio::test]
    async fn the_end_of_a_pairing_revokes_its_library() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();

        assert!(!record_pairing_end(&db, None, None).await.unwrap());
        assert!(!record_pairing_end(&db, Some("  "), None).await.unwrap());
        assert!(record_pairing_end(&db, Some(A), None).await.unwrap());

        assert_eq!(readers(&db).await.pinned_key(A), None);
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn clear_empties_the_list_and_its_state() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        revoke(&db, A, "pairing ended").await.unwrap();
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();

        clear(&db).await.unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), None);
        assert_eq!(rotation_due(&db).await.unwrap(), None);
        // The bootstrap runs again after a reset.
        reconcile(
            &db,
            &[follower(1, B, "active", Some(&key(2)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        assert_eq!(readers(&db).await.pinned_key(B), Some(key(2).as_str()));
    }

    // ── reconciliation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn the_bootstrap_lets_every_active_follower_in_once() {
        let db = test_db().await;
        set_directory(&db, true, true).await;
        let listed_at = now_rfc3339();

        reconcile(
            &db,
            &[
                follower(1, A, "active", Some(&key(1))),
                follower(2, B, "active", None),
            ],
            &listed_at,
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), Some(key(1).as_str()));
        // No key, nothing to pin: B cannot be a reader.
        assert_eq!(list.pinned_key(B), None);

        // Once only: a follower that shows up later is not let in by it.
        reconcile(
            &db,
            &[
                follower(1, A, "active", Some(&key(1))),
                follower(3, C, "active", Some(&key(3))),
            ],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        assert_eq!(readers(&db).await.pinned_key(C), None);
    }

    #[tokio::test]
    async fn the_bootstrap_prefers_the_pairing_key() {
        let db = test_db().await;
        set_directory(&db, false, true).await;
        pair(&db, A, &key(1), "accepted").await;

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(9)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), Some(key(1).as_str()));
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(9)))),
            ReaderStatus::KeyMismatch
        );
    }

    #[tokio::test]
    async fn a_pairing_the_owner_restores_lets_the_peer_back_in() {
        // Removed then paired again by the owner while its follow stayed
        // active on the hub: no pending request, so no approval. The
        // restored pairing itself must bring it back.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap(); // bootstrap done
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        record_pairing_end(&db, Some(A), None).await.unwrap();
        pair(&db, A, &key(4), "accepted").await;
        restore_pairing(&db, Some(A), None).await.unwrap();

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(4)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), Some(key(4).as_str()));
    }

    #[tokio::test]
    async fn a_pairing_key_never_replaces_a_different_pin() {
        // A peers row can be created by a message the hub could have
        // deposited: it is no reason to move a pin. A reinstalled reader
        // goes through a removal and a new pairing, which revokes first.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        pair(&db, A, &key(5), "accepted").await;

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(5)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), Some(key(1).as_str()));
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(5)))),
            ReaderStatus::KeyMismatch
        );
    }

    #[tokio::test]
    async fn a_peer_row_alone_never_makes_a_new_reader() {
        // After the bootstrap, a pairing nobody approved a follow for (it
        // may be a deposit the hub forged) does not let anyone in.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        pair(&db, C, &key(7), "accepted").await;

        reconcile(
            &db,
            &[follower(9, C, "active", Some(&key(7)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(C), None);
        assert_eq!(
            list.status(&follower(9, C, "active", Some(&key(7)))),
            ReaderStatus::Unknown
        );
    }

    #[tokio::test]
    async fn a_pairing_end_before_the_bootstrap_is_kept() {
        // First launch with the hub unreachable, a contact removed before
        // any followers list could be read: the removal must survive the
        // bootstrap that runs later.
        let db = test_db().await;
        set_directory(&db, false, true).await;

        assert!(record_pairing_end(&db, Some(A), None).await.unwrap());
        assert!(pairing_ended(&db, Some(A), None).await.unwrap());
        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), None);
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::NotReader
        );
        // A wrap handed out before the list existed may still be out there.
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn an_admission_never_lifts_an_ended_pairing() {
        // An approval admitted before the owner removed the peer can land
        // after: it must not undo the removal.
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();

        assert!(!enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap());

        assert!(pairing_ended(&db, Some(A), None).await.unwrap());
        assert_eq!(readers(&db).await.pinned_key(A), None);
        assert_eq!(wraps_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_restored_pairing_can_be_admitted_again() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();
        restore_pairing(&db, Some(A), None).await.unwrap();

        assert!(
            enrol(&db, A, &key(1), ReaderSource::Approval)
                .await
                .unwrap()
        );

        assert_eq!(readers(&db).await.pinned_key(A), Some(key(1).as_str()));
    }

    #[tokio::test]
    async fn a_rolled_back_pairing_end_records_nothing() {
        // Callers tie the record to the deletion of the peer row.
        use sea_orm::TransactionTrait;
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();

        let txn = db.begin().await.unwrap();
        record_pairing_end(&txn, Some(A), None).await.unwrap();
        txn.rollback().await.unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), Some(key(1).as_str()));
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn deleting_a_peer_ends_its_pairing_in_the_same_stroke() {
        let db = test_db().await;
        let peer = pair(&db, A, &key(1), "accepted").await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();

        delete_peer_ending_pairing(&db, &peer).await.unwrap();

        assert!(
            peer::Entity::find_by_id(peer.id)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(pairing_ended(&db, Some(A), None).await.unwrap());
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn a_pairing_end_that_cannot_be_recorded_keeps_the_peer() {
        let db = test_db().await;
        let peer = pair(&db, A, &key(1), "accepted").await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        db.execute_unprepared(
            "CREATE TRIGGER refuse_revocation BEFORE UPDATE ON catalog_readers
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .await
        .unwrap();

        assert!(delete_peer_ending_pairing(&db, &peer).await.is_err());

        assert!(
            peer::Entity::find_by_id(peer.id)
                .one(&db)
                .await
                .unwrap()
                .is_some(),
            "the deletion is rolled back with the record"
        );
        assert_eq!(readers(&db).await.pinned_key(A), Some(key(1).as_str()));
    }

    #[tokio::test]
    async fn an_ended_pairing_is_never_lifted_by_a_pairing_alone() {
        // The ex-peer pairs itself back in (a request without its uuid, the
        // uuid filled in later by its own signed manifest): the peers row
        // looks like a pairing, but the owner restored nothing.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        record_pairing_end(&db, Some(A), Some("ed-a"))
            .await
            .unwrap();
        pair(&db, A, &key(1), "accepted").await;

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), None);
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::NotReader
        );
        assert!(pairing_ended(&db, Some(A), None).await.unwrap());
    }

    #[tokio::test]
    async fn an_ended_pairing_is_recognised_by_its_signing_key_without_a_uuid() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), Some("ed-a"))
            .await
            .unwrap();

        assert!(pairing_ended(&db, None, Some("ed-a")).await.unwrap());
        assert!(pairing_ended(&db, None, Some(" ed-a ")).await.unwrap());
        assert!(!pairing_ended(&db, None, Some("ed-b")).await.unwrap());
        assert!(!pairing_ended(&db, None, None).await.unwrap());
    }

    #[tokio::test]
    async fn an_ended_pairing_is_recognised_whatever_the_uuid_case() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();

        let shouted = format!("  {}  ", A.to_uppercase());
        assert!(pairing_ended(&db, Some(&shouted), None).await.unwrap());
    }

    #[tokio::test]
    async fn the_end_of_a_pairing_with_an_upper_case_uuid_revokes_the_reader() {
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();

        assert!(
            record_pairing_end(&db, Some(&A.to_uppercase()), None)
                .await
                .unwrap()
        );

        assert_eq!(readers(&db).await.pinned_key(A), None);
    }

    #[tokio::test]
    async fn restoring_a_pairing_lifts_only_that_mark() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), Some("ed-a"))
            .await
            .unwrap();
        record_pairing_end(&db, Some(B), Some("ed-b"))
            .await
            .unwrap();

        restore_pairing(&db, Some(A), None).await.unwrap();

        assert!(!pairing_ended(&db, Some(A), None).await.unwrap());
        assert!(!pairing_ended(&db, None, Some("ed-a")).await.unwrap());
        assert!(pairing_ended(&db, Some(B), None).await.unwrap());
        // Restoring is not admitting: the reader comes back only through a
        // reconciliation that sees the pairing and an active follow.
        assert_eq!(readers(&db).await.pinned_key(A), None);
    }

    #[tokio::test]
    async fn restoring_by_signing_key_lifts_the_mark_of_a_uuid_less_request() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), Some("ed-a"))
            .await
            .unwrap();

        restore_pairing(&db, None, Some("ed-a")).await.unwrap();

        assert!(!pairing_ended(&db, Some(A), None).await.unwrap());
    }

    #[tokio::test]
    async fn restoring_a_pairing_that_never_ended_writes_nothing() {
        let db = test_db().await;

        restore_pairing(&db, Some(C), Some("ed-c")).await.unwrap();

        assert!(!readers(&db).await.has_row(C));
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_pairing_end_after_a_departure_dates_the_revocation_anew() {
        // Otherwise a reconciliation holding a list read between the two
        // would see an old revocation and could let the ex-peer back.
        let db = test_db().await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        revoke(&db, A, REASON_FOLLOWER_LEFT).await.unwrap();
        let between = now_rfc3339();

        record_pairing_end(&db, Some(A), None).await.unwrap();

        assert!(revoked_since(&db, A, &between).await.unwrap());
    }

    #[tokio::test]
    async fn an_ex_peer_is_held_back_until_the_owner_restores_the_pairing() {
        let db = test_db().await;
        let ex = pair(&db, A, &key(1), "pending").await;
        let other = pair(&db, B, &key(2), "pending").await;
        record_pairing_end(&db, Some(A), None).await.unwrap();

        assert!(held_back(&db, &ex).await.unwrap());
        assert!(!held_back(&db, &other).await.unwrap());
        // Accepted without the owner (its uuid filled in after the fact):
        // the mark still stands, so it is still held back.
        let mut accepted = ex.clone();
        accepted.connection_status = "accepted".to_string();
        assert!(held_back(&db, &accepted).await.unwrap());

        restore_pairing(&db, Some(A), None).await.unwrap();
        assert!(!held_back(&db, &ex).await.unwrap());
    }

    #[tokio::test]
    async fn every_admission_makes_wraps_due_until_they_go_out() {
        let db = test_db().await;
        assert_eq!(wraps_due(&db).await.unwrap(), None);

        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        let through = wraps_due(&db).await.unwrap().unwrap();
        // Another reader comes in while that batch is under way.
        enrol(&db, B, &key(2), ReaderSource::Approval)
            .await
            .unwrap();
        mark_wrapped(&db, through).await.unwrap();

        let through = wraps_due(&db).await.unwrap().expect("still due");
        mark_wrapped(&db, through).await.unwrap();
        assert_eq!(wraps_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_public_library_lets_its_active_followers_in() {
        // Followers of a library listed without approval become active
        // without any approval; when the owner later requires approval,
        // they must still read.
        let db = test_db().await;
        set_directory(&db, true, false).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap(); // bootstrap done

        reconcile(
            &db,
            &[follower(7, C, "active", Some(&key(3)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        assert_eq!(readers(&db).await.pinned_key(C), Some(key(3).as_str()));
    }

    #[tokio::test]
    async fn an_approval_gated_library_lets_no_one_in_by_following() {
        let db = test_db().await;
        set_directory(&db, true, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap(); // bootstrap done

        reconcile(
            &db,
            &[follower(7, C, "active", Some(&key(3)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(C), None);
        // Not shut out either: the decision keeps the catalog in clear.
        assert_eq!(
            list.status(&follower(7, C, "active", Some(&key(3)))),
            ReaderStatus::Unknown
        );
    }

    #[tokio::test]
    async fn a_reader_that_no_longer_follows_is_revoked() {
        let db = test_db().await;
        set_directory(&db, true, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        enrol(&db, B, &key(2), ReaderSource::Approval)
            .await
            .unwrap();

        reconcile(
            &db,
            &[
                follower(2, B, "active", Some(&key(2))),
                // Pending is not following.
                follower(1, A, "pending", Some(&key(1))),
            ],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), None);
        assert_eq!(list.pinned_key(B), Some(key(2).as_str()));
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn a_reader_seen_leaving_comes_back_when_it_follows_again_with_its_key() {
        // A hub round trip (prod, staging, prod) or a follow the hub left
        // out for a while: the reader the owner let in is let back in, with
        // the key that was pinned. The departure still rotated the key.
        let db = test_db().await;
        set_directory(&db, true, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        assert_eq!(readers(&db).await.pinned_key(A), None);

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::Reader
        );
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn a_reader_back_with_another_key_is_a_mismatch() {
        let db = test_db().await;
        set_directory(&db, true, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(2)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), None);
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(2)))),
            ReaderStatus::KeyMismatch
        );
    }

    // ── a key that changed under its pin (rule (b)) ──────────────────────

    #[tokio::test]
    async fn a_key_that_changed_counts_no_departure() {
        // A reinstalled reader, or a key the hub substituted: excluded from
        // the wraps, but the owner withdrew no trust, so nothing to rotate,
        // and the pin stays for the day the hub returns it again.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();

        for _ in 0..2 {
            reconcile(
                &db,
                &[follower(1, A, "active", Some(&key(2)))],
                &now_rfc3339(),
            )
            .await
            .unwrap();
        }

        let list = readers(&db).await;
        assert_eq!(list.pinned_key(A), Some(key(1).as_str()));
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(2)))),
            ReaderStatus::KeyMismatch
        );
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_reader_is_back_once_the_hub_returns_its_pinned_key() {
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        reconcile(&db, &[follower(1, A, "active", None)], &now_rfc3339())
            .await
            .unwrap();

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        assert_eq!(
            readers(&db)
                .await
                .status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::Reader
        );
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    fn follower_with_wrap(id: i64, node: &str, x25519: &str, wrap: Option<&str>) -> HubFollow {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "follower_node_id": node,
            "followed_node_id": "owner-node",
            "status": "active",
            "created_at": "2026-01-01T00:00:00Z",
            "follower_x25519_public_key": x25519,
            "encrypted_catalog_key": wrap,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_reader_back_without_its_wrap_makes_a_wrap_batch_due() {
        // The wrap was withdrawn while the hub returned another key; once
        // the pinned key is back, an unchanged catalog skips the push
        // locally, so the batch must be due for the wrap to go out again.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        mark_wrapped(&db, wraps_due(&db).await.unwrap().unwrap())
            .await
            .unwrap();

        reconcile(
            &db,
            &[follower_with_wrap(1, A, &key(1), Some("wrap"))],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        assert_eq!(wraps_due(&db).await.unwrap(), None, "holds its wrap");

        reconcile(
            &db,
            &[follower_with_wrap(1, A, &key(1), None)],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        let due = wraps_due(&db).await.unwrap();
        assert!(due.is_some(), "a reader without its wrap");

        // Asked once: a second read before the batch keeps the same count.
        reconcile(
            &db,
            &[follower_with_wrap(1, A, &key(1), Some(""))],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        assert_eq!(wraps_due(&db).await.unwrap(), due);
    }

    #[tokio::test]
    async fn a_follower_whose_key_changed_makes_no_wrap_batch_due() {
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        mark_wrapped(&db, wraps_due(&db).await.unwrap().unwrap())
            .await
            .unwrap();

        reconcile(
            &db,
            &[follower_with_wrap(1, A, &key(2), None)],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        assert_eq!(wraps_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_reinstalled_follower_is_back_after_following_again_and_an_approval() {
        // A directory follower that is no paired peer: it unfollows (the
        // departure is counted), asks again, the owner approves, and the
        // new key is pinned.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(2)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        reconcile(
            &db,
            &[follower(2, A, "pending", Some(&key(2)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));

        let (pin, source) = key_for_approval(&db, A, Some(&key(2)))
            .await
            .unwrap()
            .unwrap();
        assert!(enrol(&db, A, &pin, source).await.unwrap());

        assert_eq!(
            readers(&db)
                .await
                .status(&follower(2, A, "active", Some(&key(2)))),
            ReaderStatus::Reader
        );
    }

    #[tokio::test]
    async fn a_reinstalled_paired_peer_stays_out_until_paired_again() {
        // Its peers row still holds the key of the old pairing, which
        // outranks the hub's: an approval pins that one again, and only a
        // removal and a new pairing let the new key in.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        let peer = pair(&db, A, &key(1), "accepted").await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();

        let (pin, source) = key_for_approval(&db, A, Some(&key(2)))
            .await
            .unwrap()
            .unwrap();
        enrol(&db, A, &pin, source).await.unwrap();
        assert_eq!(
            readers(&db)
                .await
                .status(&follower(1, A, "active", Some(&key(2)))),
            ReaderStatus::KeyMismatch
        );

        delete_peer_ending_pairing(&db, &peer).await.unwrap();
        pair(&db, A, &key(2), "accepted").await;
        restore_pairing(&db, Some(A), None).await.unwrap();
        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(2)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        assert_eq!(
            readers(&db)
                .await
                .status(&follower(1, A, "active", Some(&key(2)))),
            ReaderStatus::Reader
        );
    }

    #[tokio::test]
    async fn a_list_read_before_a_departure_never_undoes_it() {
        let db = test_db().await;
        set_directory(&db, true, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();
        let stale = now_rfc3339();
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();

        reconcile(&db, &[follower(1, A, "active", Some(&key(1)))], &stale)
            .await
            .unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), None);
    }

    #[tokio::test]
    async fn a_list_read_before_an_approval_never_revokes_it() {
        // A sync read the followers, then the owner approved A, then the
        // sync reconciled with its older list: A is missing from it only
        // because the list predates the approval.
        let db = test_db().await;
        set_directory(&db, true, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        let listed_at = now_rfc3339();
        enrol(&db, A, &key(1), ReaderSource::Approval)
            .await
            .unwrap();

        reconcile(&db, &[], &listed_at).await.unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), Some(key(1).as_str()));
        assert_eq!(rotation_due(&db).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_list_read_before_a_pairing_ended_never_lets_it_back_in() {
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        let peer = pair(&db, A, &key(1), "accepted").await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        let listed_at = now_rfc3339();
        // The pairing ends after the list was read, and the peer row is
        // still there for a moment (the caller deletes it next).
        record_pairing_end(&db, Some(A), None).await.unwrap();

        reconcile(&db, &[follower(1, A, "active", Some(&key(1)))], &listed_at)
            .await
            .unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), None);
        drop(peer);
    }

    #[tokio::test]
    async fn an_ex_peer_still_following_stays_out() {
        // The case seen in the 2026-10-21 recette: the owner removed the
        // contact, the ex-peer's device had not unfollowed yet.
        let db = test_db().await;
        set_directory(&db, false, true).await;
        let peer = pair(&db, A, &key(1), "accepted").await;
        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();
        assert_eq!(readers(&db).await.pinned_key(A), Some(key(1).as_str()));

        record_pairing_end(&db, Some(A), None).await.unwrap();
        peer::Entity::delete_by_id(peer.id).exec(&db).await.unwrap();
        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        let list = readers(&db).await;
        assert_eq!(
            list.status(&follower(1, A, "active", Some(&key(1)))),
            ReaderStatus::NotReader
        );
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
    }

    async fn peer_row(db: &DatabaseConnection, uuid: Option<&str>, ed25519: &str) -> peer::Model {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set("peer".to_string()),
            url: Set(format!("relay://{ed25519}")),
            library_uuid: Set(uuid.map(str::to_string)),
            public_key: Set(Some(ed25519.to_string())),
            x25519_public_key: Set(Some(key(1))),
            key_exchange_done: Set(true),
            auto_approve: Set(true),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_ended_pairing_is_recognised_whatever_the_signing_key_case() {
        // Hex decoding takes either case: so must the mark.
        let db = test_db().await;
        record_pairing_end(&db, Some(A), Some("abcd01"))
            .await
            .unwrap();

        assert!(pairing_ended(&db, None, Some("ABCD01")).await.unwrap());

        restore_pairing(&db, None, Some(" AbCd01 ")).await.unwrap();
        assert!(!pairing_ended(&db, Some(A), None).await.unwrap());
    }

    #[tokio::test]
    async fn an_ended_pairing_without_a_uuid_is_marked_by_its_signing_key() {
        // Never a reader (the list is keyed by uuid), so nothing to rotate,
        // but it must not pair itself back in.
        let db = test_db().await;
        let peer = peer_row(&db, None, "ed-x").await;

        delete_peer_ending_pairing(&db, &peer).await.unwrap();

        assert!(pairing_ended(&db, None, Some("ed-x")).await.unwrap());
        assert_eq!(rotation_due(&db).await.unwrap(), None);
        restore_pairing(&db, None, Some("ed-x")).await.unwrap();
        assert!(!pairing_ended(&db, None, Some("ed-x")).await.unwrap());
    }

    #[tokio::test]
    async fn a_peer_that_left_is_off_the_list_without_a_mark() {
        // The other library ended the pairing: this owner decided nothing,
        // so nothing waits for it, but the key it could read is rotated.
        let db = test_db().await;
        let peer = peer_row(&db, Some(A), "ed-a").await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();

        delete_peer_that_left(&db, &peer).await.unwrap();

        assert!(
            peer::Entity::find_by_id(peer.id)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(readers(&db).await.pinned_key(A), None);
        assert_eq!(rotation_due(&db).await.unwrap(), Some(1));
        assert!(!pairing_ended(&db, Some(A), Some("ed-a")).await.unwrap());
    }

    #[tokio::test]
    async fn a_peer_that_left_and_pairs_again_is_let_back_in() {
        let db = test_db().await;
        set_directory(&db, false, true).await;
        reconcile(&db, &[], &now_rfc3339()).await.unwrap();
        let peer = peer_row(&db, Some(A), "ed-a").await;
        enrol(&db, A, &key(1), ReaderSource::Paired).await.unwrap();
        delete_peer_that_left(&db, &peer).await.unwrap();
        peer_row(&db, Some(A), "ed-a").await;

        reconcile(
            &db,
            &[follower(1, A, "active", Some(&key(1)))],
            &now_rfc3339(),
        )
        .await
        .unwrap();

        assert_eq!(readers(&db).await.pinned_key(A), Some(key(1).as_str()));
    }

    #[tokio::test]
    async fn approving_the_follow_of_an_unrestored_ex_peer_lets_nobody_in() {
        // Accepted without the owner (an identity it claimed after the
        // fact), then auto-approved as a paired peer: no key to wrap for.
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();
        pair(&db, A, &key(1), "accepted").await;

        assert_eq!(key_for_approval(&db, A, Some(&key(2))).await.unwrap(), None);
    }

    #[tokio::test]
    async fn approving_the_follow_of_an_ex_peer_no_longer_paired_is_the_owner_s_call() {
        // No pairing left to vouch for it: only the owner's own approval of
        // the request reaches here, as for any stranger.
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();

        assert_eq!(
            key_for_approval(&db, A, Some(&key(2))).await.unwrap(),
            Some((key(2), ReaderSource::Approval))
        );
    }

    #[tokio::test]
    async fn the_paired_peer_auto_approval_is_refused_for_an_ended_pairing() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();

        assert!(admit_approval(&db, Some(A), false).await.is_err());
        assert!(
            admit_approval(&db, Some(&A.to_uppercase()), false)
                .await
                .is_err()
        );
        assert!(pairing_ended(&db, Some(A), None).await.unwrap());
    }

    #[tokio::test]
    async fn the_auto_approval_goes_through_for_any_other_library() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();

        assert!(admit_approval(&db, Some(B), false).await.is_ok());
    }

    #[tokio::test]
    async fn an_auto_approval_that_names_no_library_is_refused() {
        // Nothing to check the mark against.
        let db = test_db().await;

        assert!(admit_approval(&db, None, false).await.is_err());
    }

    #[tokio::test]
    async fn the_owner_approving_an_ex_peer_restores_its_pairing() {
        let db = test_db().await;
        record_pairing_end(&db, Some(A), None).await.unwrap();
        pair(&db, A, &key(1), "accepted").await;

        admit_approval(&db, Some(A), true).await.unwrap();

        assert!(!pairing_ended(&db, Some(A), None).await.unwrap());
        assert_eq!(
            key_for_approval(&db, A, Some(&key(2))).await.unwrap(),
            Some((key(1), ReaderSource::Paired))
        );
    }
}
