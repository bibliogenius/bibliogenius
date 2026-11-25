//! Withdrawing this installation's registration on the hub (ADR-073 D4).
//!
//! The order matters. The device goes silent first: the gate is closed
//! before anything is asked of the hub, so a hub that does not answer
//! changes nothing to the silence. Then the hub is asked to delete what it
//! holds, the profile (with its catalog, covers, follows and borrow
//! requests) and the relay mailbox. These two requests go through the gated
//! functions every other request uses, with the pass of
//! [`hub_gate::with_withdrawal_pass`]; nothing else gets out.
//!
//! When the hub cannot be reached or fails to answer, the local rows that
//! carry the tokens of the deletion are kept, unused behind the closed gate,
//! and the same call is made again later. Everything else is removed from
//! the device. A refused token whose recovery is refused too, or that has
//! no recovery code to try, is final: the rows go, nothing is asked again,
//! and the outcome is "not confirmed".
//!
//! The caller names the registration it wants deleted. Rows that do not
//! match are dropped without a request: a restored backup can bring back the
//! registration of another library, and its token must never delete that
//! library's profile.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

use crate::services::account_session_service;
use crate::services::e2ee_transport::E2eeTransportError;
use crate::services::hub_catalog_key_store::CatalogKeyStore;
use crate::services::hub_directory_service::{HubDirectoryError, HubDirectoryService};
use crate::services::hub_gate;
use crate::services::relay_session::relay_config_write_lock;
use crate::services::relay_transport::RelayTransport;

/// The registration the caller read on this device and wants deleted on
/// the hub. A part left out is not deleted there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubRegistration {
    /// The library's id on the hub (`hub_directory_config.node_id`).
    pub node_id: Option<String>,
    /// The relay mailbox of this device (`my_relay_config.mailbox_uuid`).
    pub mailbox_uuid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalOutcome {
    /// The hub holds nothing of this registration any more, and neither
    /// does the device.
    Done,
    /// The device is silent, but the hub could not be asked to delete
    /// everything: call again later with the same registration.
    Pending,
    /// The device is silent and holds nothing any more, but the hub refused
    /// the write token, and the recovery code too or there was none: whether
    /// it still holds the profile cannot be told from here, and asking again
    /// would get the same answer. Nothing is left to retry; the user is told.
    Unconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithdrawalError {
    /// The encrypted account goes through the hub: the user signs out
    /// first. Nothing was changed.
    AccountSignedIn,
    /// The device could not be read or written.
    Database(String),
}

impl std::fmt::Display for WithdrawalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccountSignedIn => f.write_str("this device is signed in to an account"),
            Self::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for WithdrawalError {}

/// What became of one deletion asked of the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Deletion {
    /// Deleted, or the hub no longer knows it.
    Gone,
    /// The hub could not be asked: try again later.
    Unanswered,
    /// The hub refused every credential this device holds. Final: the same
    /// request would be refused again.
    Refused,
}

/// Withdraw the registration `named` (see the module documentation).
///
/// `poll_lock` is the relay poller's lock, when a poller runs in this
/// process: a cycle in progress may be recreating the mailbox, and the
/// relay row is only read once that cycle is over.
pub async fn withdraw(
    db: &DatabaseConnection,
    poll_lock: Option<&tokio::sync::Mutex<()>>,
    hub: &HubDirectoryService,
    key_store: &CatalogKeyStore,
    named: &HubRegistration,
) -> Result<WithdrawalOutcome, WithdrawalError> {
    if account_session_service::exists(db)
        .await
        .map_err(|e| WithdrawalError::Database(format!("{e:?}")))?
    {
        return Err(WithdrawalError::AccountSignedIn);
    }

    // Silent from here on, whatever the hub answers.
    hub_gate::set_open(false);

    // No poll cycle runs past this point: the gate stops the next ones.
    let _poll = match poll_lock {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let _relay_write = relay_config_write_lock().lock().await;

    let registration = HubDirectoryService::get_config(db)
        .await
        .map_err(|e| WithdrawalError::Database(e.to_string()))?;
    let relay = crate::api::relay::get_my_relay_config(db).await;

    let profile_named = registration
        .as_ref()
        .is_some_and(|row| named.node_id.as_deref() == Some(row.node_id.as_str()));
    let mailbox_named = relay
        .as_ref()
        .is_some_and(|row| named.mailbox_uuid.as_deref() == Some(row.mailbox_uuid.as_str()));

    let (profile, mailbox) = hub_gate::with_withdrawal_pass(async {
        let profile = if profile_named {
            delete_profile_on_hub(hub, db).await
        } else {
            Deletion::Gone
        };
        let mailbox = match relay.as_ref() {
            Some(row) if mailbox_named => {
                delete_mailbox_on_hub(&row.relay_url, &row.mailbox_uuid, &row.read_token).await
            }
            _ => Deletion::Gone,
        };
        (profile, mailbox)
    })
    .await;

    // A row is kept only for the token of a deletion still to be made:
    // not once the hub refused it for good.
    if profile != Deletion::Unanswered && registration.is_some() {
        // The covers this run uploaded go too: the hub deleted them with
        // the profile, and a later registration must send them again.
        hub.forget_registration(db, key_store)
            .await
            .map_err(|e| WithdrawalError::Database(e.to_string()))?;
    }
    // A refused profile is final and leaves nothing to ask again, so a
    // mailbox that did not answer in the same run is not retried either:
    // it goes with the hub's own expiry. Kept for a retry, it would end the
    // withdrawal as "done" and hide the refusal.
    let refused = profile == Deletion::Refused;
    if (mailbox != Deletion::Unanswered || refused) && relay.is_some() {
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM my_relay_config".to_owned(),
        ))
        .await
        .map_err(|e| WithdrawalError::Database(e.to_string()))?;
    }

    if refused {
        tracing::warn!("Hub registration withdrawn on this device, hub deletion not confirmed");
        Ok(WithdrawalOutcome::Unconfirmed)
    } else if profile == Deletion::Unanswered || mailbox == Deletion::Unanswered {
        tracing::warn!("Hub registration withdrawn on this device, hub deletion pending");
        Ok(WithdrawalOutcome::Pending)
    } else {
        tracing::info!("Hub registration withdrawn");
        Ok(WithdrawalOutcome::Done)
    }
}

/// The hub refuses a token it does not know (401) and has nothing left
/// under an id that is gone (404).
fn already_gone(status: u16) -> bool {
    matches!(status, 401 | 404)
}

async fn delete_profile_on_hub(hub: &HubDirectoryService, db: &DatabaseConnection) -> Deletion {
    match hub.delete_profile(db).await {
        Ok(()) | Err(HubDirectoryError::NotRegistered) => Deletion::Gone,
        Err(HubDirectoryError::Hub(404, _)) => Deletion::Gone,
        // A refused token is not a deleted profile: the local token may be
        // out of date (a row brought back by a restore) while the profile
        // still stands.
        Err(HubDirectoryError::Hub(401, _)) => renew_token_then_delete(hub, db).await,
        Err(e) => {
            tracing::warn!("Hub profile deletion not confirmed: {e}");
            Deletion::Unanswered
        }
    }
}

/// The write token was refused. The recovery code, when this device holds
/// one, gets a fresh token from the hub if the profile still exists; the
/// deletion is then asked again.
///
/// A hub that refuses the code too proves nothing: it answers the same to
/// a profile that is gone and to a code it rotated in an answer this
/// device never received (it changes the token and the code at each
/// recovery). That refusal is final, the same code would be refused again:
/// the deletion is reported as not confirmed, never as done, and is not
/// asked again. A hub that fails to answer is another matter, and is asked
/// again later. Without any code, nothing on this device can speak for a
/// profile: there is nothing left to try, and the refusal is reported the
/// same way, as not confirmed.
async fn renew_token_then_delete(hub: &HubDirectoryService, db: &DatabaseConnection) -> Deletion {
    let registration = match HubDirectoryService::get_config(db).await {
        Ok(Some(row)) => row,
        Ok(None) => return Deletion::Gone,
        Err(e) => {
            tracing::warn!("Local hub registration unreadable: {e}");
            return Deletion::Unanswered;
        }
    };
    let Some(recovery_code) = registration.recovery_code.filter(|code| !code.is_empty()) else {
        return Deletion::Refused;
    };
    match hub.recover(db, &registration.node_id, &recovery_code).await {
        Ok(_) => {}
        // The hub's own refusal of a recovery code. Any other status may
        // come from something in between, and is asked again.
        Err(HubDirectoryError::Hub(401, _)) => return Deletion::Refused,
        Err(e) => {
            tracing::warn!("Hub token renewal not confirmed: {e}");
            return Deletion::Unanswered;
        }
    }
    match hub.delete_profile(db).await {
        Ok(()) | Err(HubDirectoryError::NotRegistered) => Deletion::Gone,
        Err(HubDirectoryError::Hub(status, _)) if already_gone(status) => Deletion::Gone,
        Err(e) => {
            tracing::warn!("Hub profile deletion not confirmed after a token renewal: {e}");
            Deletion::Unanswered
        }
    }
}

async fn delete_mailbox_on_hub(relay_url: &str, mailbox_uuid: &str, read_token: &str) -> Deletion {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("Relay mailbox deletion not sent: {e}");
            return Deletion::Unanswered;
        }
    };
    match RelayTransport::delete_mailbox(&client, relay_url, mailbox_uuid, read_token).await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() || already_gone(status.as_u16()) {
                Deletion::Gone
            } else {
                tracing::warn!("Relay mailbox deletion answered {status}");
                Deletion::Unanswered
            }
        }
        Err(E2eeTransportError::HubSilenced) => {
            // Cannot happen under the pass; never read as a success.
            tracing::error!("Relay mailbox deletion refused by the hub gate");
            Deletion::Unanswered
        }
        Err(e) => {
            tracing::warn!("Relay mailbox deletion not confirmed: {e}");
            Deletion::Unanswered
        }
    }
}
