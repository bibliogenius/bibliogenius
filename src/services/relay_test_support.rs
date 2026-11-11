//! Two libraries and a fake hub, for tests that follow a relayed message from
//! one device to the other through the real poller and handlers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    Set,
};
use wiremock::matchers::path_regex;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use crate::crypto::envelope::{ClearMessage, EncryptedEnvelope};
use crate::crypto::identity::NodeIdentity;
use crate::infrastructure::AppState;
use crate::models::{book, copy, installation_profile, notification, p2p_outgoing_request};
use crate::models::{p2p_request, peer, relay_config};
use crate::services::identity_service::IdentityService;
use crate::services::nudge_events::NudgeSource;
use crate::services::relay_poller::poll_once;
use crate::services::relay_transport::RelayTransport;

pub(crate) const MINUTE: i64 = 60;
pub(crate) const HOUR: i64 = 60 * MINUTE;
pub(crate) const DAY: i64 = 24 * HOUR;

pub(crate) const ISBN: &str = "9782070360024";

/// The hub refuses any relay blob above this size (`RelayController::MAX_BLOB_SIZE`).
pub(crate) const HUB_MAX_BLOB_SIZE: usize = 64 * 1024;
pub(crate) const TITLE: &str = "Le Livre";

#[derive(Default)]
struct Mailboxes {
    next_id: i64,
    messages: HashMap<String, Vec<(i64, String)>>,
    deposits: HashMap<String, usize>,
    rejected: HashMap<String, usize>,
    acks_failing: bool,
    latency: Duration,
}

#[derive(Clone, Default)]
struct HubResponder(Arc<Mutex<Mailboxes>>);

impl Respond for HubResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        // api/relay/mailbox/{uuid}/messages[/{id}]
        let segments: Vec<&str> = request.url.path().split('/').collect();
        let mailbox = segments[4].to_string();
        let mut hub = self.0.lock().expect("hub lock");
        let latency = hub.latency;
        let response = match request.method.as_str() {
            "GET" => {
                let messages: Vec<serde_json::Value> = hub
                    .messages
                    .get(&mailbox)
                    .map(|queue| {
                        queue
                            .iter()
                            .map(|(id, blob)| serde_json::json!({ "id": id, "blob": blob }))
                            .collect()
                    })
                    .unwrap_or_default();
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "messages": messages }))
            }
            "POST" if request.body.len() > HUB_MAX_BLOB_SIZE => {
                *hub.rejected.entry(mailbox).or_default() += 1;
                ResponseTemplate::new(413).set_body_json(serde_json::json!({
                    "error": format!("Blob exceeds maximum size of {HUB_MAX_BLOB_SIZE} bytes")
                }))
            }
            "POST" => {
                hub.next_id += 1;
                let id = hub.next_id;
                let blob = base64::engine::general_purpose::STANDARD.encode(&request.body);
                hub.messages
                    .entry(mailbox.clone())
                    .or_default()
                    .push((id, blob));
                *hub.deposits.entry(mailbox).or_default() += 1;
                ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": id }))
            }
            "DELETE" if hub.acks_failing => ResponseTemplate::new(500),
            "DELETE" => {
                let id: i64 = segments[6].parse().expect("message id");
                if let Some(queue) = hub.messages.get_mut(&mailbox) {
                    queue.retain(|(queued, _)| *queued != id);
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
            }
            _ => ResponseTemplate::new(405),
        };
        response.set_delay(latency)
    }
}

/// A hub that keeps mailboxes as the real relay does: first in, first out.
pub(crate) struct FakeHub {
    server: MockServer,
    mailboxes: HubResponder,
}

impl FakeHub {
    pub(crate) async fn start() -> Self {
        let server = MockServer::start().await;
        let mailboxes = HubResponder::default();
        Mock::given(path_regex(r"^/api/relay/mailbox/[^/]+/messages(/\d+)?$"))
            .respond_with(mailboxes.clone())
            .mount(&server)
            .await;
        Self { server, mailboxes }
    }

    pub(crate) fn set_acks_failing(&self, failing: bool) {
        self.mailboxes.0.lock().expect("hub lock").acks_failing = failing;
    }

    /// Delay every answer, as a network round trip would.
    pub(crate) fn set_latency(&self, latency: Duration) {
        self.mailboxes.0.lock().expect("hub lock").latency = latency;
    }

    /// Messages waiting in a mailbox.
    pub(crate) fn waiting(&self, device: &Device) -> usize {
        self.mailboxes
            .0
            .lock()
            .expect("hub lock")
            .messages
            .get(&device.mailbox)
            .map_or(0, Vec::len)
    }

    /// Messages ever deposited in a mailbox, collected or not.
    pub(crate) fn deposited(&self, device: &Device) -> usize {
        self.mailboxes
            .0
            .lock()
            .expect("hub lock")
            .deposits
            .get(&device.mailbox)
            .copied()
            .unwrap_or(0)
    }

    /// Deposits refused for exceeding the blob size limit.
    pub(crate) fn rejected(&self, device: &Device) -> usize {
        self.mailboxes
            .0
            .lock()
            .expect("hub lock")
            .rejected
            .get(&device.mailbox)
            .copied()
            .unwrap_or(0)
    }

    /// The envelopes waiting in a mailbox, oldest first, left in place.
    pub(crate) fn envelopes(&self, device: &Device) -> Vec<EncryptedEnvelope> {
        let hub = self.mailboxes.0.lock().expect("hub lock");
        hub.messages
            .get(&device.mailbox)
            .map(|queue| {
                queue
                    .iter()
                    .map(|(_, blob)| {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(blob)
                            .expect("base64 blob");
                        serde_json::from_slice(&bytes).expect("envelope")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every request the hub received, whatever the route.
    pub(crate) async fn requests_seen(&self) -> usize {
        self.server
            .received_requests()
            .await
            .expect("recorded requests")
            .len()
    }

    /// Deposit an envelope untouched, the way a hub could serve it twice.
    pub(crate) fn deposit_envelope(&self, device: &Device, envelope: &EncryptedEnvelope) {
        let bytes = serde_json::to_vec(envelope).expect("serialize envelope");
        let mut hub = self.mailboxes.0.lock().expect("hub lock");
        hub.next_id += 1;
        let id = hub.next_id;
        let blob = base64::engine::general_purpose::STANDARD.encode(bytes);
        hub.messages
            .entry(device.mailbox.clone())
            .or_default()
            .push((id, blob));
        *hub.deposits.entry(device.mailbox.clone()).or_default() += 1;
    }
}

/// One library: its own database, identity and mailbox.
pub(crate) struct Device {
    pub(crate) db: DatabaseConnection,
    pub(crate) state: AppState,
    pub(crate) x25519: x25519_dalek::PublicKey,
    ed_hex: String,
    x_hex: String,
    pub(crate) uuid: String,
    pub(crate) mailbox: String,
    hub_url: String,
}

impl Device {
    pub(crate) async fn new(hub: &FakeHub) -> Self {
        let db = crate::db::init_db("sqlite::memory:")
            .await
            .expect("init db");
        let uuid = uuid::Uuid::new_v4().to_string();
        let identity_service = IdentityService::new(db.clone());
        identity_service.init(&uuid).await.expect("init identity");
        let state = AppState::with_identity_service(db.clone(), Arc::new(identity_service));
        let identity: &NodeIdentity = state.crypto_service().expect("crypto").identity();
        let x25519 = identity.x25519_public_key();
        let ed_hex = hex::encode(identity.verifying_key().to_bytes());
        let x_hex = hex::encode(identity.x25519_public_key().as_bytes());
        let mailbox = uuid::Uuid::new_v4().to_string();
        let hub_url = hub.server.uri();
        relay_config::ActiveModel {
            id: Set(1),
            relay_url: Set(hub_url.clone()),
            mailbox_uuid: Set(mailbox.clone()),
            read_token: Set(format!("read-{mailbox}")),
            write_token: Set(format!("write-{mailbox}")),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(&db)
        .await
        .expect("insert relay config");
        Self {
            db,
            state,
            x25519,
            ed_hex,
            x_hex,
            uuid,
            mailbox,
            hub_url,
        }
    }

    /// Record `other` as an accepted peer reachable only through the relay.
    pub(crate) async fn knows(&self, other: &Device) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        peer::ActiveModel {
            name: Set("Peer".to_string()),
            url: Set(format!("relay://{}", other.ed_hex)),
            library_uuid: Set(Some(other.uuid.clone())),
            public_key: Set(Some(other.ed_hex.clone())),
            x25519_public_key: Set(Some(other.x_hex.clone())),
            key_exchange_done: Set(true),
            relay_url: Set(Some(other.hub_url.clone())),
            mailbox_id: Set(Some(other.mailbox.clone())),
            relay_write_token: Set(Some(format!("write-{}", other.mailbox))),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .expect("insert peer")
        .id
    }

    pub(crate) async fn poll(&self) {
        poll_once(&self.state, NudgeSource::Manual)
            .await
            .expect("poll");
    }

    /// Seal `message` for `to` and leave it in its mailbox.
    pub(crate) async fn deposit(&self, to: &Device, message: &ClearMessage) {
        let crypto = self.state.crypto_service().expect("crypto").clone();
        RelayTransport::new(Some(crypto))
            .send(
                &to.hub_url,
                &to.mailbox,
                &format!("write-{}", to.mailbox),
                &to.x25519,
                message,
            )
            .await
            .expect("deposit");
    }

    /// Open the messages `from` left in this mailbox, skipping the first
    /// `skip`. Opening records each nonce, so an envelope opened once cannot
    /// be opened again: pass the count already read.
    pub(crate) fn read_from(&self, hub: &FakeHub, from: &Device, skip: usize) -> Vec<ClearMessage> {
        let sender = crate::services::crypto_service::PeerInfo {
            verifying_key: ed25519_dalek::VerifyingKey::from_bytes(
                &hex::decode(&from.ed_hex)
                    .expect("hex")
                    .try_into()
                    .expect("32 bytes"),
            )
            .expect("verifying key"),
            x25519_public: from.x25519,
        };
        let crypto = self.state.crypto_service().expect("crypto");
        hub.envelopes(self)
            .iter()
            .skip(skip)
            .map(|envelope| {
                crypto
                    .open_relayed(envelope, std::slice::from_ref(&sender))
                    .expect("open")
                    .0
            })
            .collect()
    }

    /// `count` public books with the fields a real catalog carries, each with
    /// its own uuid (incompressible, like the real ids).
    pub(crate) async fn add_catalog(&self, count: usize) {
        let now = chrono::Utc::now().to_rfc3339();
        let rows = (0..count).map(|i| book::ActiveModel {
            id: Set(uuid::Uuid::new_v4().to_string()),
            title: Set(format!("Titre du livre numero {i}")),
            isbn: Set(Some(format!("978207{i:07}"))),
            publisher: Set(Some("Gallimard".to_string())),
            publication_year: Set(Some(1950 + (i % 70) as i32)),
            cover_url: Set(Some(format!(
                "https://covers.openlibrary.org/b/isbn/978207{i:07}-M.jpg"
            ))),
            owned: Set(true),
            private: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        });
        book::Entity::insert_many(rows)
            .exec(&self.db)
            .await
            .expect("insert catalog");
    }

    pub(crate) fn seal(&self, to: &Device, message: &ClearMessage) -> EncryptedEnvelope {
        self.state
            .crypto_service()
            .expect("crypto")
            .seal(&to.x25519, message)
            .expect("seal")
    }

    /// A message sealed `age_secs` ago, asking for its answer in this mailbox.
    pub(crate) fn request(
        &self,
        message_type: &str,
        payload: serde_json::Value,
        age_secs: i64,
    ) -> ClearMessage {
        ClearMessage {
            correlation_id: Some(uuid::Uuid::new_v4().to_string()),
            reply_to_mailbox: Some(self.mailbox.clone()),
            reply_to_write_token: Some(format!("write-{}", self.mailbox)),
            ..message(message_type, payload, age_secs)
        }
    }

    /// The borrower side of a loan request: its pending row and the message.
    pub(crate) async fn ask_for_book(
        &self,
        lender_peer_id: i32,
        outgoing_id: &str,
        age_secs: i64,
    ) -> ClearMessage {
        let now = chrono::Utc::now().to_rfc3339();
        p2p_outgoing_request::ActiveModel {
            id: Set(outgoing_id.to_string()),
            to_peer_id: Set(lender_peer_id),
            book_isbn: Set(ISBN.to_string()),
            book_title: Set(TITLE.to_string()),
            status: Set("pending".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .expect("insert outgoing request");
        self.request(
            "loan_request",
            serde_json::json!({
                "book_isbn": ISBN,
                "book_title": TITLE,
                "requester_request_id": outgoing_id,
            }),
            age_secs,
        )
    }

    /// A book on the shelf, with one copy in the given status.
    pub(crate) async fn add_book(&self, copy_status: &str) {
        self.add_titled_book(ISBN, TITLE, copy_status).await;
    }

    pub(crate) async fn add_titled_book(&self, isbn: &str, title: &str, copy_status: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        let book_id = book::ActiveModel {
            title: Set(title.to_string()),
            isbn: Set(Some(isbn.to_string())),
            owned: Set(true),
            created_at: Set(now.clone()),
            updated_at: Set(now.clone()),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .expect("insert book")
        .id;
        let library_id = crate::utils::library_helpers::resolve_library_id(&self.db)
            .await
            .expect("library");
        copy::ActiveModel {
            book_id: Set(book_id),
            library_id: Set(library_id),
            status: Set(copy_status.to_string()),
            is_temporary: Set(false),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
        .expect("insert copy");
    }

    pub(crate) async fn enable_auto_approve(&self) {
        installation_profile::Entity::update_many()
            .col_expr(
                installation_profile::Column::EnabledModules,
                sea_orm::sea_query::Expr::value(r#"["auto_approve_loans"]"#),
            )
            .exec(&self.db)
            .await
            .expect("enable auto approve");
    }

    /// Incoming requests as (borrower's request id, status), oldest first.
    pub(crate) async fn incoming(&self) -> Vec<(Option<String>, String)> {
        let mut rows = p2p_request::Entity::find()
            .all(&self.db)
            .await
            .expect("incoming requests");
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        rows.into_iter()
            .map(|r| (r.requester_request_id, r.status))
            .collect()
    }

    pub(crate) async fn incoming_id(&self, requester_request_id: &str) -> String {
        p2p_request::Entity::find()
            .filter(p2p_request::Column::RequesterRequestId.eq(requester_request_id))
            .one(&self.db)
            .await
            .expect("query")
            .expect("incoming request")
            .id
    }

    pub(crate) async fn outgoing_status(&self, id: &str) -> Option<String> {
        p2p_outgoing_request::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .expect("outgoing request")
            .map(|r| r.status)
    }

    pub(crate) async fn notifications(&self) -> Vec<String> {
        notification::Entity::find()
            .all(&self.db)
            .await
            .expect("notifications")
            .into_iter()
            .map(|n| n.event_type)
            .collect()
    }

    pub(crate) async fn loans(&self) -> u64 {
        crate::models::loan::Entity::find()
            .count(&self.db)
            .await
            .expect("loans")
    }

    /// Copies borrowed from a peer, as the borrower records them.
    pub(crate) async fn borrowed_copies(&self) -> u64 {
        copy::Entity::find()
            .filter(copy::Column::LenderPeerId.is_not_null())
            .count(&self.db)
            .await
            .expect("borrowed copies")
    }

    pub(crate) async fn peer_row(&self, id: i32) -> Option<peer::Model> {
        peer::Entity::find_by_id(id)
            .one(&self.db)
            .await
            .expect("peer")
    }
}

/// A message sealed `age_secs` ago. A negative age dates it in the future.
pub(crate) fn message(
    message_type: &str,
    payload: serde_json::Value,
    age_secs: i64,
) -> ClearMessage {
    ClearMessage {
        message_type: message_type.to_string(),
        payload,
        timestamp: chrono::Utc::now().timestamp() - age_secs,
        message_id: uuid::Uuid::new_v4().to_string(),
        correlation_id: None,
        reply_to_mailbox: None,
        reply_to_write_token: None,
    }
}

/// Alice the borrower and Bruno the lender, paired, with a book on Bruno's shelf.
pub(crate) struct Pair {
    pub(crate) hub: FakeHub,
    pub(crate) alice: Device,
    pub(crate) bruno: Device,
    /// Bruno's row on Alice's device.
    pub(crate) bruno_on_alice: i32,
    /// Alice's row on Bruno's device.
    pub(crate) alice_on_bruno: i32,
}

pub(crate) async fn paired() -> Pair {
    let hub = FakeHub::start().await;
    let alice = Device::new(&hub).await;
    let bruno = Device::new(&hub).await;
    let bruno_on_alice = alice.knows(&bruno).await;
    let alice_on_bruno = bruno.knows(&alice).await;
    Pair {
        hub,
        alice,
        bruno,
        bruno_on_alice,
        alice_on_bruno,
    }
}
