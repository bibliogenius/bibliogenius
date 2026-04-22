#![allow(clippy::needless_update)] // SeaORM ActiveModels require ..Default::default()
use crate::models::{operation_log, peer, peer_book, peer_gamification_stats};
use axum::{
    extract::{Json, Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use futures::future::join_all;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, Set,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info};
use url::Url;

/// Validate URL to prevent SSRF (OWASP A10).
///
/// Blocks:
/// - Non-HTTP/HTTPS schemes (file://, ftp://, javascript:, etc.)
/// - Loopback (127.0.0.0/8, ::1)
/// - Link-local (169.254.0.0/16, fe80::/10) - includes AWS metadata 169.254.169.254
/// - Multicast (224.0.0.0/4, ff00::/8)
/// - Unspecified (0.0.0.0, ::)
/// - Broadcast (255.255.255.255)
/// - "localhost" hostname
///
/// Allows:
/// - Private networks (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16) for P2P LAN use
pub fn validate_url(url_str: &str) -> Result<String, String> {
    let url = Url::parse(url_str).map_err(|_| "Invalid URL format".to_string())?;

    // 1. Check Scheme
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Only HTTP/HTTPS schemes allowed".to_string());
    }

    // 2. Check Host
    match url.host() {
        Some(url::Host::Domain("localhost")) => {
            return Err("Localhost access is blocked".to_string());
        }
        Some(url::Host::Ipv4(ip)) => {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            // Link-local: 169.254.0.0/16 (includes AWS metadata endpoint 169.254.169.254)
            let octets = ip.octets();
            if octets[0] == 169 && octets[1] == 254 {
                return Err("Link-local addresses blocked".to_string());
            }
            if ip.is_multicast() {
                return Err("Multicast addresses blocked".to_string());
            }
            if ip.is_unspecified() {
                return Err("Unspecified address blocked".to_string());
            }
            // Broadcast: 255.255.255.255
            if octets == [255, 255, 255, 255] {
                return Err("Broadcast address blocked".to_string());
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if ip.is_loopback() {
                return Err("Loopback addresses blocked".to_string());
            }
            if ip.is_multicast() {
                return Err("Multicast addresses blocked".to_string());
            }
            if ip.is_unspecified() {
                return Err("Unspecified address blocked".to_string());
            }
            // IPv6 link-local: fe80::/10
            let segments = ip.segments();
            if (segments[0] & 0xffc0) == 0xfe80 {
                return Err("Link-local addresses blocked".to_string());
            }
        }
        None => {
            return Err("URL must have a host".to_string());
        }
        _ => {}
    }

    Ok(url.to_string())
}

/// Look up a peer by URL, tolerating the trailing-slash discrepancy between
/// how URLs are stored at pairing time (raw, un-normalized) and how they are
/// presented by callers (sometimes slash, sometimes not).
async fn find_peer_by_url(
    db: &DatabaseConnection,
    url: &str,
) -> Result<Option<peer::Model>, StatusCode> {
    let trimmed = url.trim_end_matches('/').to_string();
    let with_slash = format!("{trimmed}/");
    peer::Entity::find()
        .filter(
            Condition::any()
                .add(peer::Column::Url.eq(&trimmed))
                .add(peer::Column::Url.eq(&with_slash)),
        )
        .one(db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Ensure `url` refers to a peer already registered in the local DB — strict.
///
/// SSRF defense for peer-proxy endpoints: validate_url() blocks loopback and
/// link-local, but RFC1918 ranges are allowed for LAN peer discovery. Without
/// this second check, a caller could proxy through cover_proxy to probe any
/// service reachable on the local network (router admin, NAS, printers).
/// Requiring a DB match constrains peer_url to URLs vetted by the user via
/// pairing.
///
/// Use this variant for endpoints that have no legitimate "unsaved mDNS peer"
/// flow (e.g. cover_proxy, which fetches binary payloads from a URL that must
/// be user-approved). For endpoints with a legitimate mDNS fallback (browse a
/// neighbor's library before pairing), use `ensure_registered_peer_or_mdns`.
pub async fn ensure_registered_peer(
    db: &DatabaseConnection,
    url: &str,
) -> Result<peer::Model, StatusCode> {
    match ensure_registered_peer_or_mdns(db, url, false).await? {
        Some(p) => Ok(p),
        None => {
            // Unreachable: allow_unregistered_lan=false forces Err on absent.
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// Ensure `url` refers to a peer already registered, with optional mDNS
/// fallback for endpoints that must accept unsaved LAN peers (ADR-026).
///
/// Returns:
/// - `Ok(Some(peer))` when the URL matches a registered peer row.
/// - `Ok(None)` when the URL is unknown AND `allow_unregistered_lan=true`.
///   A `warn!(target = "ssrf:mdns", ...)` entry is emitted so the audit
///   trail captures every fallback traversal.
/// - `Err(StatusCode::FORBIDDEN)` when the URL is unknown AND
///   `allow_unregistered_lan=false` (strict mode, matches the original
///   `ensure_registered_peer` contract).
///
/// Callers receiving `Ok(None)` MUST treat the peer as untrusted: skip
/// outgoing-request tracking, cache enrichment, and any operation that
/// relies on a stable peer identity.
pub async fn ensure_registered_peer_or_mdns(
    db: &DatabaseConnection,
    url: &str,
    allow_unregistered_lan: bool,
) -> Result<Option<peer::Model>, StatusCode> {
    match find_peer_by_url(db, url).await? {
        Some(p) => Ok(Some(p)),
        None => {
            let safe: String = url.chars().take(128).collect();
            if allow_unregistered_lan {
                tracing::warn!(
                    target: "ssrf:mdns",
                    "peer-proxy fallback: unregistered URL allowed via mDNS path (url={safe})"
                );
                Ok(None)
            } else {
                tracing::warn!(
                    target: "ssrf",
                    "peer-proxy rejected: peer not registered (url={safe})"
                );
                Err(StatusCode::FORBIDDEN)
            }
        }
    }
}

/// Create a safe HTTP client with restricted redirects and timeouts
pub(crate) fn get_safe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none()) // Disable redirects to prevent bypass
        .build()
        .unwrap_or_default()
}

/// Query params for the cover-proxy endpoint.
#[derive(Deserialize)]
pub struct CoverProxyQuery {
    pub peer_url: String,
    pub book_id: i32,
}

/// GET /api/peers/cover-proxy?peer_url={url}&book_id={id}
///
/// Proxies a cover image fetch through the local Rust backend so that
/// Flutter does not make direct HTTP calls to the peer (which fail on
/// iOS/macOS due to firewall, ATS, or NAT issues).
pub async fn cover_proxy(
    State(db): State<DatabaseConnection>,
    axum::extract::Query(params): axum::extract::Query<CoverProxyQuery>,
) -> Result<axum::response::Response, StatusCode> {
    let peer_url = validate_url(&params.peer_url).map_err(|_| StatusCode::BAD_REQUEST)?;
    ensure_registered_peer(&db, &peer_url).await?;
    let peer_url = peer_url.trim_end_matches('/');
    let url = format!("{}/api/books/{}/cover", peer_url, params.book_id);

    let client = get_safe_client();
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if !resp.status().is_success() {
        return Err(StatusCode::NOT_FOUND);
    }

    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    // Cap response size at 10 MB to prevent memory exhaustion from a
    // malicious or misconfigured peer streaming an oversized payload.
    const MAX_COVER_BYTES: usize = 10 * 1024 * 1024;

    if let Some(cl) = resp.content_length()
        && cl as usize > MAX_COVER_BYTES
    {
        return Err(StatusCode::BAD_GATEWAY);
    }

    let bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    if bytes.len() > MAX_COVER_BYTES {
        return Err(StatusCode::BAD_GATEWAY);
    }

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, "public, max-age=3600")
        .body(axum::body::Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Translate localhost URLs to Docker service names for inter-container communication
/// Examples:
/// - http://localhost:8001 -> http://bibliogenius-a:8000
/// - http://localhost:8002 -> http://bibliogenius-b:8000
fn translate_url_for_docker(url: &str) -> String {
    if url.contains("localhost:8001") {
        url.replace("localhost:8001", "bibliogenius-a:8000")
    } else if url.contains("localhost:8002") {
        url.replace("localhost:8002", "bibliogenius-b:8000")
    } else {
        url.to_string()
    }
}

/// Check if the `connection_validation` module is enabled in installation profile
async fn is_connection_validation_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile;

    if let Ok(Some(profile)) = installation_profile::Entity::find().one(db).await {
        return profile.enabled_modules.contains("connection_validation");
    }
    false
}

/// Check if `auto_approve_loans` module is enabled in installation profile
pub(crate) async fn is_auto_approve_loans_enabled(db: &DatabaseConnection) -> bool {
    use crate::models::installation_profile;

    if let Ok(Some(profile)) = installation_profile::Entity::find().one(db).await {
        return profile.enabled_modules.contains("auto_approve_loans");
    }
    false
}

/// Check if a specific peer is approved for access.
/// Returns true if connection_validation is disabled OR if the peer has connection_status == "accepted".
async fn is_peer_approved(db: &DatabaseConnection, peer: &peer::Model) -> bool {
    if !is_connection_validation_enabled(db).await {
        return true;
    }
    peer.connection_status == "accepted"
}

/// Result of a successful loan acceptance on the lender side.
pub(crate) struct LoanAcceptResult {
    pub lender_name: String,
    pub due_date: String,
    pub book_isbn: Option<String>,
    pub book_title: String,
    pub book_cover_url: Option<String>,
}

/// Resolve the effective loan duration (in days) for a given book.
///
/// Reads from `loan_settings` (global default + per-book override).
/// Falls back to 21 days if the settings table is unreachable.
async fn resolve_loan_duration_days(db: &DatabaseConnection, book_id: i32) -> i64 {
    let repo = crate::infrastructure::SeaOrmLoanSettingsRepository::new(db.clone());
    use crate::domain::LoanSettingsRepository;
    match repo.get_effective_duration(book_id).await {
        Ok(days) => days as i64,
        Err(e) => {
            tracing::warn!("Failed to read loan settings, using 21-day default: {e}");
            21
        }
    }
}

/// Core acceptance logic shared by plaintext and E2EE auto-approve paths.
///
/// Finds book/copy, creates contact/loan, updates copy status and request status.
/// Does NOT send notifications to the borrower (caller handles that).
pub(crate) async fn perform_loan_acceptance(
    db: &DatabaseConnection,
    request_id: &str,
    book_isbn: &str,
    book_title: &str,
    peer: &peer::Model,
) -> Result<LoanAcceptResult, String> {
    use crate::models::{book, contact, copy, loan, p2p_request};

    // 1. Find Book by ISBN (fallback to title)
    let book = match book::Entity::find()
        .filter(book::Column::Isbn.eq(book_isbn))
        .one(db)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => match book::Entity::find()
            .filter(book::Column::Title.eq(book_title))
            .one(db)
            .await
        {
            Ok(Some(b)) => b,
            _ => {
                return Err(format!(
                    "Book not found (ISBN: '{book_isbn}', Title: '{book_title}')"
                ));
            }
        },
        Err(e) => return Err(format!("DB error finding book: {e}")),
    };

    // 2. Find available copy
    let copy = match copy::Entity::find()
        .filter(copy::Column::BookId.eq(book.id))
        .filter(copy::Column::Status.eq("available"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        _ => return Err("No available copies".to_string()),
    };

    // 3. Find or create contact for peer
    let contact = match contact::Entity::find()
        .filter(contact::Column::Name.eq(&peer.name))
        .filter(contact::Column::Type.eq("Library"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            let lib_id = crate::utils::library_helpers::resolve_library_id(db)
                .await
                .map_err(|e| format!("No library: {e}"))?;
            let new_contact = contact::ActiveModel {
                r#type: Set("Library".to_string()),
                name: Set(peer.name.clone()),
                library_owner_id: Set(lib_id),
                is_active: Set(true),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            new_contact
                .insert(db)
                .await
                .map_err(|e| format!("Failed to create contact: {e}"))?
        }
        Err(e) => return Err(format!("DB error finding contact: {e}")),
    };

    // 4. Create loan
    let lib_id = crate::utils::library_helpers::resolve_library_id(db)
        .await
        .map_err(|e| format!("No library: {e}"))?;
    let duration_days = resolve_loan_duration_days(db, book.id).await;
    let due = Utc::now() + chrono::Duration::days(duration_days);
    let loan = loan::ActiveModel {
        copy_id: Set(copy.id),
        contact_id: Set(contact.id),
        library_id: Set(lib_id),
        loan_date: Set(Utc::now().to_rfc3339()),
        due_date: Set(due.to_rfc3339()),
        status: Set("active".to_string()),
        created_at: Set(Utc::now().to_rfc3339()),
        updated_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    loan::Entity::insert(loan)
        .exec(db)
        .await
        .map_err(|e| format!("Failed to create loan: {e}"))?;

    // 5. Update copy status
    info!("Auto-approve: Updating copy {} status to 'loaned'", copy.id);
    let mut active_copy: copy::ActiveModel = copy.into();
    active_copy.status = Set("loaned".to_string());
    active_copy
        .update(db)
        .await
        .map_err(|e| format!("Failed to update copy status: {e}"))?;

    // 6. Update request status to accepted
    if let Ok(Some(req)) = p2p_request::Entity::find_by_id(request_id).one(db).await {
        let mut active_req: p2p_request::ActiveModel = req.into();
        active_req.status = Set("accepted".to_string());
        active_req.updated_at = Set(Utc::now().to_rfc3339());
        let _ = active_req.update(db).await;
    }

    // 7. Get lender name
    let lender_name = match crate::models::library::Entity::find_by_id(1).one(db).await {
        Ok(Some(lib)) => lib.name,
        _ => "Unknown Library".to_string(),
    };

    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    Ok(LoanAcceptResult {
        lender_name,
        due_date: due.format("%Y-%m-%d").to_string(),
        book_isbn: book.isbn,
        book_title: book.title,
        // Loan-accept result is forwarded to the borrower through an E2EE
        // envelope that may travel via hub relay, so use the relay-safe
        // variant: local paths without a hub prefix are stripped to None.
        book_cover_url: crate::models::Book::safe_cover_url_for_relay(
            book.cover_url.as_deref(),
            book.id,
            Some(book.updated_at.as_str()),
            hub_prefix.as_deref(),
        ),
    })
}

// ============ SHARED HELPERS: BORROWED COPY CREATION ============

/// Parameters for creating a borrowed copy on the borrower side.
pub(crate) struct BorrowedCopyParams<'a> {
    pub title: &'a str,
    pub isbn: Option<&'a str>,
    pub author: Option<&'a str>,
    pub cover_url: Option<&'a str>,
    pub lender_name: &'a str,
    pub due_date: &'a str,
}

/// Result of creating a borrowed copy.
pub(crate) struct BorrowedCopyResult {
    pub book_id: i32,
    pub copy_id: i32,
    /// `true` if an identical borrowed copy already existed (idempotency).
    pub already_existed: bool,
}

/// Find or create a book and its borrowed temporary copy on the borrower side.
///
/// Shared by `receive_loan_confirmation`, `handle_loan_confirmation` (e2ee.rs),
/// and the `loan_offer` handlers. Callers handle outgoing-request updates and
/// notifications themselves.
pub(crate) async fn create_borrowed_copy(
    db: &DatabaseConnection,
    params: &BorrowedCopyParams<'_>,
) -> Result<BorrowedCopyResult, (StatusCode, serde_json::Value)> {
    use crate::models::{book, copy};

    // 1. Find or create book
    let existing_book = if let Some(isbn_val) = params.isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn_val))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(params.title))
            .one(db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => {
            tracing::info!("Borrowed copy: book already exists id={}", b.id);

            // Update cover_url if the incoming one is HTTP and the existing
            // one is missing or a local file path (legacy borrowed books).
            if let Some(new_cover) = params.cover_url {
                let needs_update = new_cover.starts_with("http")
                    && !b
                        .cover_url
                        .as_deref()
                        .is_some_and(|u| u.starts_with("http"));
                if needs_update {
                    let mut active: book::ActiveModel = b.clone().into();
                    active.cover_url = Set(Some(new_cover.to_string()));
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    if let Err(e) = active.update(db).await {
                        tracing::warn!("Failed to update cover_url for book id={}: {e}", b.id);
                    } else {
                        tracing::info!(
                            "Updated cover_url for borrowed book id={} to HTTP URL",
                            b.id
                        );
                    }
                }
            }

            b.id
        }
        None => {
            let now = Utc::now().to_rfc3339();
            let summary_text = params.author.map(|a| format!("Auteur: {a}"));
            let new_book = book::ActiveModel {
                title: Set(params.title.to_string()),
                isbn: Set(params.isbn.map(|s| s.to_string())),
                summary: Set(summary_text),
                cover_url: Set(params.cover_url.map(|s| s.to_string())),
                owned: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            match new_book.insert(db).await {
                Ok(b) => {
                    tracing::info!("Borrowed copy: created book id={}", b.id);
                    b.id
                }
                Err(e) => {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({ "error": format!("Failed to create book: {e}") }),
                    ));
                }
            }
        }
    };

    // 2. Idempotency: skip if a borrowed temporary copy already exists
    let existing_borrowed = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("borrowed"))
        .filter(copy::Column::IsTemporary.eq(true))
        .one(db)
        .await
        .ok()
        .flatten();

    if let Some(existing) = existing_borrowed {
        tracing::info!(
            "Borrowed copy already exists (id={}) for book_id={}, skipping",
            existing.id,
            book_id
        );
        return Ok(BorrowedCopyResult {
            book_id,
            copy_id: existing.id,
            already_existed: true,
        });
    }

    // 3. Create borrowed temporary copy
    let lib_id = crate::utils::library_helpers::resolve_library_id(db)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("No library: {e}") }),
            )
        })?;

    let now = Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(lib_id),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {} jusqu'au {}",
            params.lender_name, params.due_date
        ))),
        acquisition_date: Set(Some(now.clone())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(db).await {
        Ok(c) => {
            tracing::info!("Created borrowed copy id={} for book_id={}", c.id, book_id);
            Ok(BorrowedCopyResult {
                book_id,
                copy_id: c.id,
                already_existed: false,
            })
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("Failed to create copy: {e}") }),
        )),
    }
}

// ============ LENDER-INITIATED LOAN TO PEER ============

#[derive(Debug, Deserialize)]
pub struct OfferLoanRequest {
    pub book_id: Option<i32>,
    pub book_isbn: Option<String>,
}

/// POST /api/peers/:id/offer-loan
///
/// Lender initiates a loan to a connected peer. Creates the loan locally and
/// notifies the peer via E2EE (with relay fallback) so a borrowed copy appears
/// on the borrower's device.
pub async fn offer_loan(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<OfferLoanRequest>,
) -> impl IntoResponse {
    use crate::models::{book, contact, copy, library, loan};

    let db = state.db();

    // 1. Find peer and verify connection status
    let peer = match peer::Entity::find_by_id(peer_id).one(db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };
    if peer.connection_status != "accepted" {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "Peer not connected" })),
        )
            .into_response();
    }

    // 2. Find book by ID or ISBN
    let book = if let Some(book_id) = payload.book_id {
        book::Entity::find_by_id(book_id)
            .one(db)
            .await
            .ok()
            .flatten()
    } else if let Some(ref isbn) = payload.book_isbn {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let book = match book {
        Some(b) => b,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Book not found" })),
            )
                .into_response();
        }
    };

    // 3. Find available copy (no auto-creation)
    let available_copy = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book.id))
        .filter(copy::Column::Status.eq("available"))
        .one(db)
        .await
        .ok()
        .flatten();
    let available_copy = match available_copy {
        Some(c) => c,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "No available copies" })),
            )
                .into_response();
        }
    };

    // 4. Find or create Library contact for peer
    let peer_contact = match contact::Entity::find()
        .filter(contact::Column::Name.eq(&peer.name))
        .filter(contact::Column::Type.eq("Library"))
        .one(db)
        .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
                Ok(id) => id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("No library: {e}") })),
                    )
                        .into_response();
                }
            };
            let new_contact = contact::ActiveModel {
                r#type: Set("Library".to_string()),
                name: Set(peer.name.clone()),
                library_owner_id: Set(lib_id),
                is_active: Set(true),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_contact.insert(db).await {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create contact: {e}") })),
                    )
                        .into_response();
                }
            }
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("DB error: {e}") })),
            )
                .into_response();
        }
    };

    // 5. Calculate loan duration and create loan
    let duration_days = resolve_loan_duration_days(db, book.id).await;
    let due = Utc::now() + chrono::Duration::days(duration_days);
    let due_date_str = due.format("%Y-%m-%d").to_string();

    let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("No library: {e}") })),
            )
                .into_response();
        }
    };
    let new_loan = loan::ActiveModel {
        copy_id: Set(available_copy.id),
        contact_id: Set(peer_contact.id),
        library_id: Set(lib_id),
        loan_date: Set(Utc::now().to_rfc3339()),
        due_date: Set(due.to_rfc3339()),
        status: Set("active".to_string()),
        created_at: Set(Utc::now().to_rfc3339()),
        updated_at: Set(Utc::now().to_rfc3339()),
        ..Default::default()
    };
    let loan_insert = match loan::Entity::insert(new_loan).exec(db).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create loan: {e}") })),
            )
                .into_response();
        }
    };

    // 6. Update copy status to "loaned"
    let mut active_copy: copy::ActiveModel = available_copy.into();
    active_copy.status = Set("loaned".to_string());
    if let Err(e) = active_copy.update(db).await {
        tracing::warn!("Failed to update copy status: {e}");
    }

    // 7. Create p2p_request so the return flow can find the loan
    let request_id = uuid::Uuid::new_v4().to_string();
    {
        use crate::models::p2p_request;
        let req = p2p_request::ActiveModel {
            id: Set(request_id.clone()),
            from_peer_id: Set(peer.id),
            book_isbn: Set(book.isbn.clone().unwrap_or_default()),
            book_title: Set(book.title.clone()),
            status: Set("accepted".to_string()),
            requester_request_id: Set(None),
            created_at: Set(Utc::now().to_rfc3339()),
            updated_at: Set(Utc::now().to_rfc3339()),
        };
        if let Err(e) = p2p_request::Entity::insert(req).exec(db).await {
            tracing::warn!("Failed to create p2p_request for offer_loan: {e}");
        }
    }

    // 8. Build loan_offer payload and notify peer
    let lender_name = match library::Entity::find_by_id(1).one(db).await {
        Ok(Some(lib)) => lib.name,
        _ => "Unknown Library".to_string(),
    };

    let hub_prefix = crate::models::Book::hub_cover_prefix(db).await;
    let offer_payload = json!({
        "isbn": book.isbn,
        "title": book.title,
        // Payload goes through `try_send_e2ee` (relay-capable): strip
        // unservable local paths rather than embedding a `/api/books/{id}/cover`
        // URL the borrower cannot reach from the hub relay.
        "cover_url": crate::models::Book::safe_cover_url_for_relay(
            book.cover_url.as_deref(),
            book.id,
            Some(book.updated_at.as_str()),
            hub_prefix.as_deref(),
        ),
        "lender_name": lender_name,
        "due_date": due_date_str,
        "request_id": request_id,
    });

    let mut notification_sent = false;

    match try_send_e2ee(&state, &peer, "loan_offer", offer_payload.clone()).await {
        Ok(Some(_)) => {
            tracing::info!("E2EE: loan_offer sent to {} (encrypted)", peer.name);
            notification_sent = true;
        }
        Err(e) => {
            tracing::warn!("E2EE: loan_offer error (no plaintext fallback): {e}");
        }
        Ok(None) => {
            // E2EE not available -- fall back to plaintext
            if let Ok(validated) = validate_url(&peer.url) {
                let client = get_safe_client();
                match client
                    .post(format!("{validated}/api/peers/loans/offer"))
                    .json(&offer_payload)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        tracing::info!(
                            "loan_offer plaintext sent to {}: {}",
                            peer.name,
                            resp.status()
                        );
                        notification_sent = resp.status().is_success();
                    }
                    Err(e) => {
                        tracing::warn!("Failed to send plaintext loan_offer to {}: {e}", peer.name);
                    }
                }
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "message": "Loan created",
            "loan_id": loan_insert.last_insert_id,
            "contact_id": peer_contact.id,
            "due_date": due_date_str,
            "notification_sent": notification_sent,
        })),
    )
        .into_response()
}

/// Default overall timeout for awaiting a relay response in `try_send_e2ee`.
///
/// 90s covers one full remote poller cycle (60s) plus jitter and processing,
/// so fire-and-forget request/response paths (loans, searches, syncs) keep
/// their historical behavior. Latency-sensitive callers (leaderboard refresh)
/// should use [`try_send_e2ee_with_timeout`] with a shorter bound.
pub(crate) const DEFAULT_E2EE_RELAY_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(90);

/// Try to send a message to a peer via E2EE. Returns Ok(Some(response)) if E2EE succeeded,
/// Ok(None) if E2EE is not available for this peer (caller should fall back to plaintext).
///
/// ADR-012: All message types now support relay fallback. Request-response messages
/// (search_request, book_sync_request, library_*) attach reply_to fields so the
/// responder can deposit the answer in our mailbox.
///
/// Uses [`DEFAULT_E2EE_RELAY_TIMEOUT`] (90s) for relay response await. Use
/// [`try_send_e2ee_with_timeout`] when a different bound is needed.
pub(crate) async fn try_send_e2ee(
    state: &crate::infrastructure::AppState,
    peer: &peer::Model,
    message_type: &str,
    payload: serde_json::Value,
) -> Result<Option<Option<crate::crypto::envelope::ClearMessage>>, String> {
    try_send_e2ee_with_timeout(
        state,
        peer,
        message_type,
        payload,
        DEFAULT_E2EE_RELAY_TIMEOUT,
    )
    .await
}

/// Same as [`try_send_e2ee`] but with a caller-chosen `overall_timeout` for the
/// relay response await loop. Useful when the caller can tolerate missing a
/// slow peer in exchange for faster UI feedback (e.g. leaderboard refresh
/// where a 90s wait would freeze the refresh spinner).
pub(crate) async fn try_send_e2ee_with_timeout(
    state: &crate::infrastructure::AppState,
    peer: &peer::Model,
    message_type: &str,
    payload: serde_json::Value,
    overall_timeout: std::time::Duration,
) -> Result<Option<Option<crate::crypto::envelope::ClearMessage>>, String> {
    // Check if peer supports E2EE
    if !peer.key_exchange_done {
        tracing::warn!(
            "E2EE: Skipping - peer {} key_exchange_done=false",
            peer.name
        );
        return Ok(None); // Plaintext fallback
    }

    let crypto_service = match state.crypto_service() {
        Some(svc) => svc.clone(),
        None => {
            tracing::warn!("E2EE: Skipping - CryptoService not initialized");
            return Ok(None); // Identity not ready, fallback
        }
    };

    // Parse peer's X25519 public key
    let x25519_hex = match &peer.x25519_public_key {
        Some(hex) => hex,
        None => {
            tracing::warn!(
                "E2EE: Skipping - peer {} missing x25519_public_key",
                peer.name
            );
            return Ok(None);
        }
    };
    let x_bytes = hex::decode(x25519_hex).map_err(|e| format!("Invalid x25519 key: {e}"))?;
    if x_bytes.len() != 32 {
        return Ok(None);
    }
    let x_arr: [u8; 32] = x_bytes.try_into().unwrap();
    let peer_x25519 = x25519_dalek::PublicKey::from(x_arr);

    // Parse peer's Ed25519 verifying key (for opening responses)
    let ed_hex = match &peer.public_key {
        Some(hex) => hex,
        None => return Ok(None),
    };
    let ed_bytes = hex::decode(ed_hex).map_err(|e| format!("Invalid ed25519 key: {e}"))?;
    if ed_bytes.len() != 32 {
        return Ok(None);
    }
    let ed_arr: [u8; 32] = ed_bytes.try_into().unwrap();
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&ed_arr)
        .map_err(|e| format!("Invalid ed25519 key: {e}"))?;

    let peer_info = crate::services::crypto_service::PeerInfo {
        verifying_key,
        x25519_public: peer_x25519,
    };

    let transport = crate::services::e2ee_transport::DirectTransport::new(crypto_service.clone());
    let message =
        crate::services::e2ee_transport::DirectTransport::build_message(message_type, payload);

    // Skip direct if peer recently failed and has usable relay credentials.
    // Don't skip when the write_token is gated by ADR-032, otherwise we'd
    // waste the one chance direct still has to reach the peer on LAN.
    let skip_direct = state.is_peer_direct_unreachable(peer.id)
        && peer.relay_url.is_some()
        && peer.mailbox_id.is_some()
        && peer.relay_write_token.is_some()
        && peer.relay_gate_allows_send();

    let direct_result = if skip_direct {
        tracing::info!(
            "E2EE: Skipping direct for peer {} (cached unreachable), using relay",
            peer.name,
        );
        Err(
            crate::services::e2ee_transport::E2eeTransportError::Network(
                "peer cached as unreachable".to_string(),
            ),
        )
    } else {
        transport
            .send(&peer.url, &peer_x25519, &peer_info, &message)
            .await
    };

    match direct_result {
        Ok(response) => {
            // Direct succeeded -- clear any cached failure for this peer
            state.clear_peer_direct_failed(peer.id);
            tracing::info!(
                "E2EE: Sent '{}' to peer {} ({})",
                message_type,
                peer.name,
                peer.id
            );
            Ok(Some(response))
        }
        Err(crate::services::e2ee_transport::E2eeTransportError::Network(ref net_err)) => {
            // Mark peer as unreachable so subsequent calls skip direct
            if !skip_direct {
                state.mark_peer_direct_failed(peer.id);
            }
            // Network error - peer unreachable. Try relay fallback.
            // ADR-012: All message types can now be relayed. Request-response messages
            // attach reply_to fields so responses come back via our mailbox.
            // ADR-032: Skip relay entirely when the peer's write_token has been
            // flagged stale and the retry window hasn't elapsed. This is the
            // primary flood-suppression point for broadcast + interactive sends.
            if !peer.relay_gate_allows_send() {
                tracing::info!(
                    "E2EE Relay: Skipping peer {} - write_token flagged stale (ADR-032)",
                    peer.name
                );
                return Err(format!(
                    "E2EE: peer {} unreachable (direct: {net_err}, relay: invitation stale)",
                    peer.name
                ));
            }
            if let (Some(relay_url), Some(mailbox_id), Some(write_token)) =
                (&peer.relay_url, &peer.mailbox_id, &peer.relay_write_token)
            {
                tracing::info!(
                    "E2EE: Direct failed ({}), trying relay for '{}' to peer {}",
                    net_err,
                    message_type,
                    peer.name,
                );

                // For relay messages, attach reply_to fields from our relay config
                // so the responder can deposit the answer in our mailbox.
                let mut relay_message = message.clone();
                let mut correlation_id_for_await: Option<String> = None;

                // Only await relay responses for request-response types.
                // Fire-and-forget types (loan_confirmation, status_update, etc.)
                // are deposited in the peer's mailbox but we don't block waiting.
                const RELAY_AWAIT_RESPONSE: &[&str] = &[
                    "loan_request",
                    "book_sync_request",
                    "search_request",
                    "device_sync_request",
                    "library_manifest_request",
                    "library_page_request",
                    "library_search_request",
                    "request_status_query",
                    "public_stats_request",  // ADR-022: leaderboard relay sync
                    "catalog_delta_request", // ADR-029: delta sync over relay
                    "avatar_sync_request",   // ADR-025: avatar + library_name sync over relay
                ];
                let needs_response = RELAY_AWAIT_RESPONSE.contains(&message_type);

                if let Some(my_config) = crate::api::relay::get_my_relay_config(state.db()).await {
                    let correlation_id = uuid::Uuid::new_v4().to_string();
                    relay_message.correlation_id = Some(correlation_id.clone());
                    relay_message.reply_to_mailbox = Some(my_config.mailbox_uuid.clone());
                    relay_message.reply_to_write_token = Some(my_config.write_token.clone());
                    if needs_response {
                        correlation_id_for_await = Some(correlation_id);
                    }
                }

                let relay =
                    crate::services::relay_transport::RelayTransport::new(Some(crypto_service));

                // Try relay send, with automatic retry on 404 (expired mailbox)
                let relay_send_ok = match relay
                    .send(
                        relay_url,
                        mailbox_id,
                        write_token,
                        &peer_x25519,
                        &relay_message,
                    )
                    .await
                {
                    Ok(()) => true,
                    Err(crate::services::e2ee_transport::E2eeTransportError::PeerError(
                        404,
                        ref _body,
                    )) => {
                        // Peer's mailbox expired/deleted on the hub.
                        // Try to refresh their relay credentials from /api/config.
                        tracing::warn!(
                            "E2EE Relay: Peer {} mailbox not found (404), attempting credential refresh",
                            peer.name,
                        );
                        if let Some(refreshed) =
                            refresh_peer_relay_credentials(state.db(), peer).await
                        {
                            match relay
                                .send(
                                    &refreshed.0,
                                    &refreshed.1,
                                    &refreshed.2,
                                    &peer_x25519,
                                    &relay_message,
                                )
                                .await
                            {
                                Ok(()) => {
                                    tracing::info!(
                                        "E2EE Relay: Retry succeeded for peer {} with refreshed credentials",
                                        peer.name
                                    );
                                    true
                                }
                                Err(retry_err) => {
                                    // ADR-032: still 404 after refreshed creds,
                                    // or any other terminal error. Flag the
                                    // write_token to stop the flood.
                                    mark_peer_invite_stale(state.db(), peer.id).await;
                                    if let Some(corr_id) = correlation_id_for_await {
                                        state.cancel_relay_request(&corr_id);
                                    }
                                    return Err(format!(
                                        "E2EE relay failed after credential refresh: {retry_err}"
                                    ));
                                }
                            }
                        } else {
                            tracing::warn!(
                                "E2EE Relay: Credential refresh returned nothing for peer {} (is_relay_only={})",
                                peer.name,
                                peer.url.starts_with("relay://")
                            );
                            // ADR-032: refresh exhausted LAN + hub directory;
                            // no way to recover the token without a fresh
                            // invitation. Flag and short-circuit next calls.
                            mark_peer_invite_stale(state.db(), peer.id).await;
                            if let Some(corr_id) = correlation_id_for_await {
                                state.cancel_relay_request(&corr_id);
                            }
                            return Err(format!(
                                "E2EE relay: peer {} mailbox expired, peer unreachable for credential refresh",
                                peer.name
                            ));
                        }
                    }
                    Err(relay_err) => {
                        if let Some(corr_id) = correlation_id_for_await {
                            state.cancel_relay_request(&corr_id);
                        }
                        tracing::warn!(
                            "E2EE Relay: Also failed for peer {}: {relay_err}",
                            peer.name
                        );
                        return Err(format!(
                            "E2EE send failed (direct: {net_err}, relay: {relay_err})"
                        ));
                    }
                };

                if relay_send_ok {
                    tracing::info!(
                        "E2EE Relay: Sent '{}' to peer {} via relay",
                        message_type,
                        peer.name
                    );

                    // Await the relay response with periodic polling instead
                    // of returning 202 and relying on Flutter adaptive polling.
                    // `overall_timeout` comes from the caller: legacy path uses
                    // `DEFAULT_E2EE_RELAY_TIMEOUT` (90s), latency-sensitive paths
                    // like leaderboard refresh use a shorter bound.
                    if let Some(corr_id) = correlation_id_for_await {
                        let mut rx = state.register_relay_request(corr_id.clone());
                        let start = std::time::Instant::now();

                        // Trigger immediate poll (don't wait for 60s background cycle)
                        let _ = crate::services::relay_poller::poll_once(
                            state,
                            crate::services::nudge_events::NudgeSource::Manual,
                        )
                        .await;

                        loop {
                            tokio::select! {
                                result = &mut rx => {
                                    match result {
                                        Ok(payload) => {
                                            tracing::info!(
                                                "E2EE Relay: Got response for '{}' from peer {} ({}ms)",
                                                message_type,
                                                peer.name,
                                                start.elapsed().as_millis()
                                            );
                                            let response_msg = crate::crypto::envelope::ClearMessage {
                                                message_type: format!("{message_type}_response"),
                                                payload,
                                                timestamp: chrono::Utc::now().timestamp(),
                                                message_id: uuid::Uuid::new_v4().to_string(),
                                                correlation_id: Some(corr_id),
                                                reply_to_mailbox: None,
                                                reply_to_write_token: None,
                                            };
                                            return Ok(Some(Some(response_msg)));
                                        }
                                        Err(_) => {
                                            return Ok(Some(None));
                                        }
                                    }
                                }
                                // Poll every 2s (was 5s) so responses are picked up faster
                                // when the WS nudge is unavailable (e.g. connection in progress).
                                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                                    if start.elapsed() >= overall_timeout {
                                        tracing::info!(
                                            "E2EE Relay: Timeout waiting for '{}' response from peer {} ({}s)",
                                            message_type,
                                            peer.name,
                                            start.elapsed().as_secs()
                                        );
                                        state.cancel_relay_request(&corr_id);
                                        return Ok(Some(None));
                                    }
                                    let _ = crate::services::relay_poller::poll_once(
                                        state,
                                        crate::services::nudge_events::NudgeSource::Manual,
                                    )
                                    .await;
                                }
                            }
                        }
                    }

                    return Ok(Some(None));
                }
            }

            tracing::warn!(
                "E2EE: Peer {} unreachable via LAN ({}) and has no relay credentials",
                peer.name,
                net_err,
            );
            Err(format!("E2EE send failed: network error: {net_err}"))
        }
        Err(e) => Err(format!("E2EE send failed: {e}")),
    }
}

/// Pull a peer's avatar and library name over E2EE (ADR-025).
///
/// Sends `avatar_sync_request`, waits for `avatar_sync_response`, persists
/// `peers.avatar_config` and `peers.name` when they differ from the cached
/// value. Returns `true` when at least one field changed.
///
/// Called from three trigger points:
///   1. On first-seen of an accepted relay-only peer (no cached avatar).
///   2. From Flutter after receiving a `profile_changed` WS nudge.
///   3. Opportunistically during relay poll cycles (at most once per 24h).
pub(crate) async fn try_pull_avatar_via_relay(
    state: &crate::infrastructure::AppState,
    peer_id: i32,
) -> Result<bool, String> {
    let db = state.db();

    let peer_model = peer::Entity::find_by_id(peer_id)
        .one(db)
        .await
        .map_err(|e| format!("load peer {peer_id}: {e}"))?
        .ok_or_else(|| format!("peer {peer_id} not found"))?;

    let send_result = try_send_e2ee(state, &peer_model, "avatar_sync_request", json!({})).await;

    let response = match send_result {
        Ok(Some(Some(resp))) => resp,
        Ok(Some(None)) => {
            tracing::info!(
                "avatar_sync: peer {} did not respond (likely pre-ADR-025)",
                peer_model.name
            );
            return Ok(false);
        }
        Ok(None) => {
            tracing::debug!(
                "avatar_sync: peer {} has no E2EE capability",
                peer_model.name
            );
            return Ok(false);
        }
        Err(e) => return Err(format!("try_send_e2ee: {e}")),
    };

    // `avatar_config` is either a JSON object/value or null. Serialize back
    // to a string for storage (peer.avatar_config is TEXT, matching the
    // existing piggyback path in sync_peer).
    let new_avatar: Option<String> = response
        .payload
        .get("avatar_config")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::to_string(v).ok());

    let new_name: Option<String> = response
        .payload
        .get("library_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let avatar_changed = new_avatar.is_some() && new_avatar != peer_model.avatar_config;
    let name_changed = new_name.as_ref().is_some_and(|n| n != &peer_model.name);

    if !avatar_changed && !name_changed {
        tracing::debug!("avatar_sync: peer {} already up to date", peer_model.name);
        return Ok(false);
    }

    let mut active: peer::ActiveModel = peer_model.clone().into();
    if avatar_changed {
        active.avatar_config = Set(new_avatar.clone());
    }
    if name_changed {
        active.name = Set(new_name.clone().unwrap_or_else(|| peer_model.name.clone()));
    }
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    active
        .update(db)
        .await
        .map_err(|e| format!("update peer {peer_id}: {e}"))?;

    tracing::info!(
        "avatar_sync: peer {} updated (avatar_changed={}, name_changed={})",
        peer_model.name,
        avatar_changed,
        name_changed
    );
    Ok(true)
}

/// Attempt to refresh a peer's relay credentials.
///
/// Strategy:
///   1. LAN peers: fetch `/api/config` directly (fast, no hub dependency).
///   2. Relay-only peers: query the hub directory for updated credentials.
///      The hub only returns relay fields to authenticated requesters, and
///      the caller verifies the x25519 key matches before trusting them.
///
/// Returns `Some((relay_url, mailbox_id, write_token))` on success.
/// Updates the peer record in the database.
pub async fn refresh_peer_relay_credentials(
    db: &DatabaseConnection,
    peer_model: &peer::Model,
) -> Option<(String, String, String)> {
    let (relay_url, mailbox_id, write_token) = if peer_model.url.starts_with("relay://") {
        // Relay-only: query hub directory for updated credentials
        refresh_via_hub(db, peer_model).await?
    } else {
        // LAN peer: try direct HTTP fetch first (fast, no hub dependency)
        let lan_result = refresh_via_lan(peer_model).await;
        if lan_result.is_some() {
            lan_result?
        } else if peer_model.library_uuid.is_some() {
            // LAN unreachable -- fallback to hub directory if peer is registered
            tracing::info!(
                "Relay: LAN refresh failed for peer '{}', falling back to hub directory",
                peer_model.name
            );
            refresh_via_hub(db, peer_model).await?
        } else {
            return None;
        }
    };

    if relay_url.is_empty() || mailbox_id.is_empty() || write_token.is_empty() {
        return None;
    }

    // Update peer record with fresh relay credentials. Any stale-invite flag
    // (ADR-032) is cleared at the same time since the new token is assumed
    // fresh from the hub/LAN probe.
    if let Ok(Some(existing)) = peer::Entity::find_by_id(peer_model.id).one(db).await {
        let mut active: peer::ActiveModel = existing.into();
        active.relay_url = Set(Some(relay_url.clone()));
        active.mailbox_id = Set(Some(mailbox_id.clone()));
        active.relay_write_token = Set(Some(write_token.clone()));
        active.relay_write_token_invalid_at = Set(None);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active.update(db).await;
        tracing::info!(
            "Relay: Refreshed credentials for peer '{}' (mailbox: {})",
            peer_model.name,
            mailbox_id
        );
    }

    Some((relay_url, mailbox_id, write_token))
}

/// ADR-032: Flag a peer's `relay_write_token` as invalid. Called after a
/// deposit 404 that could not be recovered by `refresh_peer_relay_credentials`.
/// Subsequent sends short-circuit via `peer.relay_gate_allows_send()` until
/// either the retry window elapses, a refresh succeeds, or the user imports
/// a fresh invitation from the peer.
pub(crate) async fn mark_peer_invite_stale(db: &DatabaseConnection, peer_id: i32) {
    if let Ok(Some(existing)) = peer::Entity::find_by_id(peer_id).one(db).await {
        let mut active: peer::ActiveModel = existing.into();
        active.relay_write_token_invalid_at = Set(Some(chrono::Utc::now().to_rfc3339()));
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        if let Err(e) = active.update(db).await {
            tracing::warn!(
                "Relay: failed to persist stale-invite flag for peer {}: {}",
                peer_id,
                e
            );
        } else {
            tracing::info!(
                "Relay: Flagged peer {} write_token as stale (ADR-032)",
                peer_id
            );
        }
    }
}

/// Refresh relay credentials via direct HTTP to the peer's LAN URL.
async fn refresh_via_lan(peer_model: &peer::Model) -> Option<(String, String, String)> {
    let client = get_safe_client();
    let config_url = format!("{}/api/config", peer_model.url.trim_end_matches('/'));
    tracing::debug!(
        "Relay: LAN refresh attempt for peer '{}' at {}",
        peer_model.name,
        config_url
    );

    let response = match client.get(&config_url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                "Relay: LAN refresh failed for peer '{}': {}",
                peer_model.name,
                e
            );
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::debug!(
            "Relay: LAN refresh for peer '{}' returned HTTP {}",
            peer_model.name,
            response.status()
        );
        return None;
    }

    let config: crate::api::setup::ConfigResponse = response.json().await.ok()?;
    Some((
        config.relay_url?,
        config.mailbox_id?,
        config.relay_write_token?,
    ))
}

/// Refresh relay credentials via the hub directory (for relay-only peers).
///
/// Queries the hub for the peer's profile using their library_uuid (node_id).
/// Verifies the x25519 key matches before trusting the returned credentials.
async fn refresh_via_hub(
    db: &DatabaseConnection,
    peer_model: &peer::Model,
) -> Option<(String, String, String)> {
    tracing::info!(
        "Relay: Hub refresh attempt for peer '{}' (node_id: {:?})",
        peer_model.name,
        peer_model.library_uuid
    );
    let hub_url =
        crate::services::hub_directory_service::HubDirectoryService::hub_base_url().ok()?;
    let peer_node_id = peer_model.library_uuid.as_deref()?;

    // Authenticate with our own write_token
    let our_config = crate::services::hub_directory_service::HubDirectoryService::get_config(db)
        .await
        .ok()
        .flatten()?;

    let client = get_safe_client();
    let url = format!("{hub_url}/api/directory/{peer_node_id}");
    let response = client
        .get(&url)
        .header(
            "Authorization",
            format!("Bearer {}", our_config.write_token),
        )
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        tracing::debug!(
            "Relay: Hub profile lookup failed for peer '{}' (status {})",
            peer_model.name,
            response.status()
        );
        return None;
    }

    let profile: crate::services::hub_directory_service::HubProfile = response.json().await.ok()?;

    // Verify x25519 key matches what we have locally to prevent
    // an attacker from redirecting messages to their own mailbox.
    if let Some(ref local_key) = peer_model.x25519_public_key
        && profile.x25519_public_key.as_deref() != Some(local_key.as_str())
    {
        tracing::warn!(
            "Relay: Hub profile x25519 key mismatch for peer '{}', rejecting credentials",
            peer_model.name
        );
        return None;
    }

    let relay_url = profile.relay_url?;
    let mailbox_id = profile.relay_mailbox_id?;
    let write_token = profile.relay_write_token?;

    tracing::info!(
        "Relay: Refreshed credentials for relay-only peer '{}' via hub (mailbox: {})",
        peer_model.name,
        mailbox_id
    );

    Some((relay_url, mailbox_id, write_token))
}

/// Bulk-approve all pending peers (called when connection_validation is toggled OFF)
pub async fn auto_approve_all_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let peers = peer::Entity::find()
        .filter(peer::Column::ConnectionStatus.eq("pending"))
        .all(&db)
        .await
        .unwrap_or_default();

    let count = peers.len();
    for p in peers {
        let mut active: peer::ActiveModel = p.into();
        active.connection_status = Set("accepted".to_string());
        active.auto_approve = Set(true);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active.update(&db).await;
    }

    tracing::info!("✅ Auto-approved {} pending peers", count);
    (
        StatusCode::OK,
        Json(json!({ "message": format!("Approved {} peers", count), "count": count })),
    )
        .into_response()
}

// ── Relay setup ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetupRelayRequest {
    pub relay_url: String,
}

/// POST /api/peers/relay/setup — Register a mailbox on a relay hub.
///
/// Calls the relay hub to create a new mailbox, then stores the config locally.
pub async fn setup_relay(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<SetupRelayRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Validate relay URL
    if let Err(e) = validate_url(&payload.relay_url) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }

    // 2. Call relay hub to create a mailbox
    let client = get_safe_client();
    let url = format!(
        "{}/api/relay/mailbox",
        payload.relay_url.trim_end_matches('/')
    );

    let response = match client.post(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Failed to reach relay hub: {e}") })),
            )
                .into_response();
        }
    };

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Relay hub returned error: {body}") })),
        )
            .into_response();
    }

    let result: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Invalid relay response: {e}") })),
            )
                .into_response();
        }
    };

    let mailbox_uuid = result
        .get("uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let read_token = result
        .get("read_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let write_token = result
        .get("write_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if mailbox_uuid.is_empty() || read_token.is_empty() || write_token.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Relay hub returned incomplete mailbox data" })),
        )
            .into_response();
    }

    // 3. Persist the new mailbox and conditionally invalidate the hub
    //    directory config. Same-URL re-setups must preserve the write_token,
    //    otherwise the next heartbeat loops on 401 against the existing hub
    //    profile that only the purged token could authenticate.
    let relay_url_for_notify = payload.relay_url.clone();

    let hub_changed = match apply_relay_setup(
        db,
        &payload.relay_url,
        &mailbox_uuid,
        &read_token,
        &write_token,
    )
    .await
    {
        Ok(changed) => changed,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to save relay config: {e}") })),
            )
                .into_response();
        }
    };

    tracing::info!("Relay: Mailbox registered");

    // Keep HUB_URL in sync so hub_directory_service uses the same hub.
    // SAFETY: single-threaded write path (same pattern as set_hub_url_ffi).
    unsafe { std::env::set_var("HUB_URL", &relay_url_for_notify) };

    if hub_changed {
        tracing::info!(
            "Relay: HUB_URL updated to {}, directory config invalidated (hub changed)",
            &relay_url_for_notify
        );
    } else {
        tracing::info!(
            "Relay: HUB_URL set to {}, directory config preserved (hub unchanged)",
            &relay_url_for_notify
        );
    }

    // Proactively notify all E2EE peers of the new mailbox credentials.
    // This prevents the window where peers have stale relay info after a hub switch.
    let state_clone = state.clone();
    let mailbox_uuid_for_notify = mailbox_uuid.clone();
    tokio::spawn(async move {
        crate::services::relay_poller::notify_peers_of_new_credentials(
            &state_clone,
            &relay_url_for_notify,
            &mailbox_uuid_for_notify,
        )
        .await;
    });

    (
        StatusCode::OK,
        Json(json!({
            "mailbox_uuid": mailbox_uuid,
            "write_token": write_token,
        })),
    )
        .into_response()
}

/// Persist a freshly-registered relay mailbox and invalidate the hub
/// directory config only when the hub URL actually changes.
///
/// Extracted from `setup_relay` so the DB-level conditional can be tested
/// without standing up a mock relay server. Returns `true` if the hub URL
/// differed from the previous config (and `hub_directory_config` was
/// therefore wiped), `false` otherwise.
async fn apply_relay_setup(
    db: &DatabaseConnection,
    relay_url: &str,
    mailbox_uuid: &str,
    read_token: &str,
    write_token: &str,
) -> Result<bool, sea_orm::DbErr> {
    use crate::models::relay_config;
    use sea_orm::ConnectionTrait;

    let previous_hub_url: Option<String> = relay_config::Entity::find()
        .one(db)
        .await?
        .map(|m| m.relay_url);

    db.execute(sea_orm::Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM my_relay_config".to_owned(),
    ))
    .await?;

    let now = chrono::Utc::now().to_rfc3339();
    relay_config::ActiveModel {
        id: Set(1),
        relay_url: Set(relay_url.to_string()),
        mailbox_uuid: Set(mailbox_uuid.to_string()),
        read_token: Set(read_token.to_string()),
        write_token: Set(write_token.to_string()),
        created_at: Set(now),
    }
    .insert(db)
    .await?;

    crate::services::relay_session::mark_mailbox_created_this_session();

    let hub_changed = previous_hub_url
        .as_deref()
        .is_some_and(|prev| crate::utils::hub_url::hub_urls_differ(prev, relay_url));

    if hub_changed {
        db.execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM hub_directory_config".to_owned(),
        ))
        .await?;
    }

    Ok(hub_changed)
}

/// GET /api/peers/relay/config — Get current relay config (if any).
pub async fn get_relay_config_endpoint(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    match crate::api::relay::get_my_relay_config(db).await {
        Some(config) => (
            StatusCode::OK,
            Json(json!({
                "relay_url": config.relay_url,
                "mailbox_uuid": config.mailbox_uuid,
                "write_token": config.write_token,
                "created_at": config.created_at,
            })),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No relay configured" })),
        )
            .into_response(),
    }
}

/// DELETE /api/peers/relay/config - Remove relay config (disconnect from hub).
///
/// Before deleting the local config, attempts to delete the mailbox on the hub
/// so it does not linger as an orphan accepting stale deposits.
pub async fn delete_relay_config_endpoint(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Read current config before deleting (need mailbox UUID + read_token for hub cleanup)
    let config = crate::api::relay::get_my_relay_config(db).await;

    // 2. Best-effort: delete the mailbox on the hub
    if let Some(ref cfg) = config {
        let url = format!("{}/api/relay/mailbox/{}", cfg.relay_url, cfg.mailbox_uuid);
        let client = get_safe_client();
        match client
            .delete(&url)
            .header("Authorization", format!("Bearer {}", cfg.read_token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("Relay: Deleted mailbox on hub");
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "Relay: Hub mailbox delete returned {} body={}",
                    status,
                    body
                );
            }
            Err(e) => {
                tracing::warn!("Relay: Failed to delete mailbox on hub: {e}");
            }
        }
    }

    // 3. Delete local config
    use sea_orm::ConnectionTrait;
    match db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM my_relay_config".to_owned(),
        ))
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Relay config removed" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to remove relay config: {e}") })),
        )
            .into_response(),
    }
}

// ── Relay library sync endpoints (ADR-012) ──────────────────────────

/// Send a library sync request to a peer via E2EE (relay or direct).
/// Returns the response payload if available, or starts async relay flow.
///
/// POST /api/peers/relay/library_request
/// Body: { "peer_id": int, "request_type": "manifest"|"page"|"search", ... }
#[derive(Deserialize)]
pub struct RelayLibraryRequest {
    pub peer_id: i32,
    pub request_type: String,
    #[serde(default)]
    pub cursor: Option<i64>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub query: Option<String>,
}

pub async fn relay_library_request(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<RelayLibraryRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find the peer
    let the_peer = match peer::Entity::find_by_id(req.peer_id).one(db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 2. Build the E2EE message type and payload
    let (message_type, payload) = match req.request_type.as_str() {
        "manifest" => ("library_manifest_request", json!({})),
        "page" => (
            "library_page_request",
            json!({
                "cursor": req.cursor,
                "limit": req.limit.unwrap_or(50),
            }),
        ),
        "search" => (
            "library_search_request",
            json!({
                "query": req.query.unwrap_or_default(),
                "limit": req.limit.unwrap_or(20),
            }),
        ),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid request_type. Use: manifest, page, search" })),
            )
                .into_response();
        }
    };

    // 3. Send via E2EE (direct or relay with reply-to)
    tracing::info!(
        "Relay library request: type='{}' peer='{}' (id={})",
        req.request_type,
        the_peer.name,
        the_peer.id,
    );

    match try_send_e2ee(&state, &the_peer, message_type, payload).await {
        Ok(Some(Some(response))) => {
            // Direct response (LAN path)
            tracing::info!(
                "Relay library request: '{}' for peer '{}' resolved via direct LAN",
                req.request_type,
                the_peer.name
            );
            (StatusCode::OK, Json(response.payload)).into_response()
        }
        Ok(Some(None)) => {
            // Sent via relay (no immediate response)
            tracing::info!(
                "Relay library request: '{}' for peer '{}' sent via relay (pending)",
                req.request_type,
                the_peer.name
            );
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "relay_pending",
                    "message": "Request sent via relay. Use poll_now to check for response.",
                })),
            )
                .into_response()
        }
        Ok(None) => {
            // E2EE not available - no plaintext fallback for library sync
            tracing::warn!(
                "Relay library request: '{}' for peer '{}' failed - E2EE not available",
                req.request_type,
                the_peer.name
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "E2EE not available for this peer" })),
            )
                .into_response()
        }
        Err(e)
            if e.contains("peer unreachable for credential refresh")
                || e.contains("failed after credential refresh") =>
        {
            // Peer's mailbox expired and we cannot refresh credentials.
            // Return 502 so the client stops retrying (circuit breaker).
            tracing::warn!(
                "Relay library request: '{}' for peer '{}' - peer unreachable (502): {}",
                req.request_type,
                the_peer.name,
                e
            );
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": "peer_unreachable",
                    "message": e,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(
                "Relay library request: '{}' for peer '{}' failed (500): {}",
                req.request_type,
                the_peer.name,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response()
        }
    }
}

/// Wait for a pending relay response by correlation_id.
///
/// POST /api/peers/relay/await_response
/// Body: { "correlation_id": "uuid", "timeout_ms": 5000 }
#[derive(Deserialize)]
pub struct AwaitRelayResponse {
    pub correlation_id: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5000
}

pub async fn await_relay_response(
    State(state): State<crate::infrastructure::AppState>,
    Json(req): Json<AwaitRelayResponse>,
) -> impl IntoResponse {
    let timeout = std::time::Duration::from_millis(req.timeout_ms.min(30_000));

    // Register a new listener (or check if one already exists)
    let rx = state.register_relay_request(req.correlation_id.clone());

    // Wait for the response with timeout
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(payload)) => (StatusCode::OK, Json(payload)).into_response(),
        Ok(Err(_)) => {
            // Sender dropped (cancelled)
            (
                StatusCode::GONE,
                Json(json!({ "error": "Request was cancelled" })),
            )
                .into_response()
        }
        Err(_) => {
            // Timeout - clean up
            state.cancel_relay_request(&req.correlation_id);
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(json!({ "status": "timeout", "message": "No response yet" })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ConnectRequest {
    name: String,
    url: String,
    public_key: Option<String>,
    /// Stable library UUID for P2P peer deduplication
    #[serde(default)]
    library_uuid: Option<String>,
    /// Ed25519 public key (hex) from the remote peer - for E2EE
    #[serde(default)]
    ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the remote peer - for E2EE
    #[serde(default)]
    x25519_public_key: Option<String>,
    /// Peer's relay hub URL
    #[serde(default)]
    relay_url: Option<String>,
    /// Peer's relay mailbox UUID
    #[serde(default)]
    mailbox_id: Option<String>,
    /// Token to write to peer's relay mailbox
    #[serde(default)]
    relay_write_token: Option<String>,
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
    struct RemoteConfigData {
        latitude: Option<f64>,
        longitude: Option<f64>,
        remote_name: Option<String>,
        library_uuid: Option<String>,
        ed25519_public_key: Option<String>,
        x25519_public_key: Option<String>,
        relay_url: Option<String>,
        mailbox_id: Option<String>,
        relay_write_token: Option<String>,
        avatar_config: Option<String>,
    }

    let (remote_data, remote_reachable) = if is_relay_only {
        // Relay-only: no remote config to fetch, all data from payload
        (
            RemoteConfigData {
                latitude: None,
                longitude: None,
                remote_name: None,
                library_uuid: None,
                ed25519_public_key: None,
                x25519_public_key: None,
                relay_url: None,
                mailbox_id: None,
                relay_write_token: None,
                avatar_config: None,
            },
            false,
        )
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
                        _ => (
                            RemoteConfigData {
                                latitude: None,
                                longitude: None,
                                remote_name: None,
                                library_uuid: None,
                                ed25519_public_key: None,
                                x25519_public_key: None,
                                relay_url: None,
                                mailbox_id: None,
                                relay_write_token: None,
                                avatar_config: None,
                            },
                            false,
                        ),
                    }
                } else {
                    (
                        RemoteConfigData {
                            latitude: None,
                            longitude: None,
                            remote_name: None,
                            library_uuid: None,
                            ed25519_public_key: None,
                            x25519_public_key: None,
                            relay_url: None,
                            mailbox_id: None,
                            relay_write_token: None,
                            avatar_config: None,
                        },
                        false,
                    )
                }
            }
            Err(_) => (
                RemoteConfigData {
                    latitude: None,
                    longitude: None,
                    remote_name: None,
                    library_uuid: None,
                    ed25519_public_key: None,
                    x25519_public_key: None,
                    relay_url: None,
                    mailbox_id: None,
                    relay_write_token: None,
                    avatar_config: None,
                },
                false,
            ),
        }
    };

    // Use provided name or fallback to remote name or "Unknown"
    let name = if !payload.name.is_empty() {
        payload.name
    } else {
        remote_data
            .remote_name
            .unwrap_or_else(|| "Unknown Library".to_string())
    };

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

    // Translate localhost URLs to Docker service names for inter-container communication
    // For relay-only peers (empty URL), use a unique placeholder to satisfy
    // the NOT NULL UNIQUE constraint on peers.url (same pattern as relay_poller).
    let docker_url = if is_relay_only {
        let unique_part = peer_library_uuid
            .as_deref()
            .or(ed25519_key.as_deref())
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        format!("relay://{unique_part}")
    } else {
        translate_url_for_docker(&payload.url)
    };
    let peer_url_for_sync = docker_url.clone(); // Clone before moving into ActiveModel

    // Upsert: find existing peer by library_uuid first (most reliable),
    // then by URL (handles port changes from hot restarts).
    // For relay-only peers (empty URL), UUID is the only reliable key.
    let mut existing = if let Some(ref uuid) = peer_library_uuid {
        peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await
    } else {
        Ok(None)
    };

    if matches!(&existing, Ok(None)) && !docker_url.is_empty() {
        existing = peer::Entity::find()
            .filter(peer::Column::Url.eq(&docker_url))
            .one(&db)
            .await;
    }

    // Relay info: prefer payload, fall back to remote config
    let relay_url = payload.relay_url.or(remote_data.relay_url);
    let mailbox_id = payload.mailbox_id.or(remote_data.mailbox_id);
    let relay_write_token = payload.relay_write_token.or(remote_data.relay_write_token);

    // Clone for relay handshake (values will be moved into ActiveModel below)
    let relay_url_for_handshake = relay_url.clone();
    let mailbox_id_for_handshake = mailbox_id.clone();
    let relay_write_token_for_handshake = relay_write_token.clone();

    let peer_id = match existing {
        Ok(Some(existing_peer)) => {
            // Update existing peer with new keys and info
            let peer_id = existing_peer.id;
            let old_library_uuid = existing_peer.library_uuid.clone();
            let mut active: peer::ActiveModel = existing_peer.into();
            active.name = Set(name);
            active.url = Set(docker_url.clone()); // Update URL (port may have changed)
            active.library_uuid = Set(peer_library_uuid.clone());
            active.public_key = Set(ed25519_key.clone());
            active.x25519_public_key = Set(x25519_key);
            active.key_exchange_done = Set(key_exchange_done);
            active.latitude = Set(remote_data.latitude);
            active.longitude = Set(remote_data.longitude);
            active.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            active.auto_approve = Set(true);
            active.connection_status = Set("accepted".to_string());
            // Store avatar config if provided
            if remote_data.avatar_config.is_some() {
                active.avatar_config = Set(remote_data.avatar_config.clone());
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
            match active.update(&db).await {
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
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
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
            match peer::Entity::insert(peer).exec(&db).await {
                Ok(res) => res.last_insert_id,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": e.to_string() })),
                    )
                        .into_response();
                }
            }
        }
    };

    // Trigger background sync of peer catalog
    let db_clone = db.clone();
    tokio::spawn(async move {
        tracing::info!("🔄 Background sync triggered for new peer {}", peer_id);
        if let Err(e) = sync_peer_internal(&db_clone, peer_id, &peer_url_for_sync).await {
            tracing::warn!("⚠️ Background sync failed for peer {}: {}", peer_id, e);
        }
    });

    // If the remote peer was unreachable (no WiFi) but we have their relay
    // credentials, tell Flutter to deposit the connection_request via native
    // HTTP (Dio). reqwest+rustls fails on iOS FFI, so the deposit is handled
    // by the Flutter caller using the native HTTP stack.
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

/// Upsert peer books cache: stores `added_at` from the owner peer so the
/// "new" badge is consistent across all viewers (no longer derived from
/// the local cache observation time). Removes books no longer in the
/// fresh list. Returns the number of books in the fresh list.
async fn upsert_peer_books_cache(
    db: &DatabaseConnection,
    peer_id: i32,
    node_id: Option<&str>,
    books: Vec<crate::models::Book>,
) -> usize {
    let now = chrono::Utc::now().to_rfc3339();
    let count = books.len();

    tracing::info!(
        "upsert_peer_books_cache: peer_id={}, incoming={} books",
        peer_id,
        count,
    );

    // 1. Load existing cached books for this peer
    let mut existing = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(db)
        .await
        .unwrap_or_default();

    // Migration: a previous bug in Book.toJson() omitted the id field, causing
    // all cached entries to have remote_book_id=0. When incoming books now carry
    // real IDs, purge the corrupted rows so the fresh upsert replaces them.
    let zero_id_count = existing.iter().filter(|e| e.remote_book_id == 0).count();
    let incoming_have_real_ids = books.iter().any(|b| matches!(b.id, Some(id) if id != 0));
    if zero_id_count > 1 && incoming_have_real_ids {
        tracing::info!(
            "upsert_peer_books_cache: peer_id={} - purging {} corrupted entries \
             (remote_book_id=0) from previous toJson bug",
            peer_id,
            zero_id_count,
        );
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(0))
            .exec(db)
            .await;
        existing.retain(|e| e.remote_book_id != 0);
    }

    let existing_map: std::collections::HashMap<i32, peer_book::Model> = existing
        .into_iter()
        .map(|e| (e.remote_book_id, e))
        .collect();

    let mut fresh_ids = std::collections::HashSet::new();

    // Guard: if incoming list is empty but we have cached books, this would
    // wipe the entire cache. Log a warning to help diagnose relay truncation.
    if count == 0 && !existing_map.is_empty() {
        tracing::warn!(
            "upsert_peer_books_cache: peer_id={} - incoming=0 but {} cached books exist, \
             skipping destructive sync (possible relay truncation)",
            peer_id,
            existing_map.len(),
        );
        return 0;
    }

    // 2. Upsert each book
    for book in books {
        let remote_id = book.id.unwrap_or(0);
        fresh_ids.insert(remote_id);

        if let Some(existing_entry) = existing_map.get(&remote_id) {
            // UPDATE: refresh metadata. `added_at` from the peer overrides any
            // stale local value (the owner is the source of truth).
            let mut active: peer_book::ActiveModel = existing_entry.clone().into();
            active.title = Set(book.title);
            active.isbn = Set(book.isbn);
            active.author = Set(book.author);
            active.cover_url = Set(book.cover_url);
            active.summary = Set(book.summary);
            active.synced_at = Set(now.clone());
            if let Some(nid) = node_id {
                active.node_id = Set(Some(nid.to_string()));
            }
            if book.added_at.is_some() {
                active.added_at = Set(book.added_at);
            }
            // Loan status: owner is authoritative. Missing `owned` means a
            // pre-073 peer or a DTO with the field stripped — fall back to
            // `true` so legacy books stay visible.
            active.owned = Set(book.owned.unwrap_or(true));
            active.available_copies = Set(book.available_copies);
            // notified_at stays unchanged
            let _ = active.update(db).await;
        } else {
            // INSERT: new book (notified_at = NULL - not yet notified)
            let cache = peer_book::ActiveModel {
                peer_id: Set(peer_id),
                remote_book_id: Set(remote_id),
                title: Set(book.title),
                isbn: Set(book.isbn),
                author: Set(book.author),
                cover_url: Set(book.cover_url),
                summary: Set(book.summary),
                synced_at: Set(now.clone()),
                node_id: Set(node_id.map(|s| s.to_string())),
                first_seen_at: Set(None),
                added_at: Set(book.added_at),
                notified_at: Set(None),
                owned: Set(book.owned.unwrap_or(true)),
                available_copies: Set(book.available_copies),
                ..Default::default()
            };
            let _ = peer_book::Entity::insert(cache).exec(db).await;
        }
    }

    // 3. Delete books no longer in the fresh list
    for (remote_id, entry) in &existing_map {
        if !fresh_ids.contains(remote_id) {
            let _ = peer_book::Entity::delete_by_id(entry.id).exec(db).await;
        }
    }

    // 4. Check un-notified books against wishlist + emit "new_books" notification.
    // Uses notified_at IS NULL instead of tracking inserts in memory, so that
    // notification dedup survives notification pruning (TTL/cap).
    let unnotified = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .filter(peer_book::Column::NotifiedAt.is_null())
        .all(db)
        .await
        .unwrap_or_default();

    if !unnotified.is_empty() {
        let new_isbns: Vec<(String, String)> = unnotified
            .iter()
            .filter_map(|pb| {
                pb.isbn
                    .as_ref()
                    .map(|isbn| (isbn.clone(), pb.title.clone()))
            })
            .collect();

        let peer_name = peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default();
        let ref_id = peer_id.to_string();

        // Wishlist matches
        if !new_isbns.is_empty() {
            crate::services::notification_service::check_wishlist_matches(
                db, &new_isbns, &peer_name, "peer", &ref_id,
            )
            .await;
        }

        // Mark all un-notified books as notified so they won't trigger again
        for pb in unnotified {
            let mut active: peer_book::ActiveModel = pb.into();
            active.notified_at = Set(Some(now.clone()));
            let _ = active.update(db).await;
        }
    }

    count
}

/// Internal sync function for background sync after connect
async fn sync_peer_internal(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
) -> Result<usize, String> {
    // Validate URL
    validate_url(peer_url).map_err(|e| format!("Invalid peer URL: {}", e))?;

    let client = get_safe_client();

    // First, check peer's config for privacy consent flags
    let config_url = format!("{}/api/config", peer_url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };

    // Distinguish "peer explicitly disallows caching" from "peer unreachable"
    let peer_reachable = peer_config.is_some();
    let allows_caching = peer_config
        .as_ref()
        .map(|c| c.allow_library_caching)
        .unwrap_or(true); // assume caching OK when unreachable - preserve cache
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));

    // Extract updated name and avatar from peer config (single DB read)
    let peer_library_name = peer_config.as_ref().map(|c| c.library_name.clone());
    let (updated_name, updated_avatar) = if let Some(config) = &peer_config {
        if let Ok(Some(p)) = peer::Entity::find_by_id(peer_id).one(db).await {
            let name = if p.name != config.library_name {
                Some(config.library_name.clone())
            } else {
                None
            };
            let avatar_json = config
                .avatar_config
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let avatar = if avatar_json != p.avatar_config {
                avatar_json
            } else {
                None
            };
            (name, avatar)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Resolve peer display name for memory score upsert
    let display_name = peer_library_name.as_deref().unwrap_or(peer_url);

    if peer_reachable && !allows_caching {
        tracing::info!(
            "Peer {} explicitly disallows library caching, clearing cache",
            peer_url
        );
        // Peer is reachable and explicitly disallows caching - clear cache
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .exec(db)
            .await;
        // Still sync gamification stats if available
        sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;
        // Still sync memory game scores
        crate::modules::memory_game::handlers::sync_peer_memory_scores(
            db,
            peer_id,
            peer_url,
            display_name,
            &client,
            peer_has_memory_game,
        )
        .await;
        // Still sync sliding puzzle scores
        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
            db,
            peer_id,
            peer_url,
            display_name,
            &client,
            peer_has_sliding_puzzle,
        )
        .await;
        // Still update last_seen (and name if changed)
        if let Ok(Some(peer)) = crate::models::peer::Entity::find_by_id(peer_id)
            .one(db)
            .await
        {
            let mut active_peer: peer::ActiveModel = peer.into();
            if let Some(ref new_name) = updated_name {
                active_peer.name = Set(new_name.clone());
                tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
            }
            if let Some(ref avatar) = updated_avatar {
                active_peer.avatar_config = Set(Some(avatar.clone()));
            }
            active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active_peer.update(db).await;
        }
        return Ok(0); // Return 0 books cached
    }

    // Fetch remote books (owned only - exclude books the peer borrowed from others)
    let url = format!("{}/api/books?owned_only=true", peer_url);

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to contact peer: {}", e))?;

    if !response.status().is_success() {
        return Err("Peer returned error".to_string());
    }

    // Parse response
    #[derive(Deserialize)]
    struct BooksResponse {
        books: Vec<crate::models::Book>,
    }

    let data: BooksResponse = response
        .json()
        .await
        .map_err(|_| "Invalid response format".to_string())?;

    // Upsert books cache (preserves first_seen_at for existing entries)
    let count = upsert_peer_books_cache(db, peer_id, None, data.books).await;

    // Sync gamification stats if both sides have the module enabled
    sync_peer_gamification_stats(db, peer_id, peer_url, &client, shares_gamification).await;

    // Sync memory game scores
    crate::modules::memory_game::handlers::sync_peer_memory_scores(
        db,
        peer_id,
        peer_url,
        display_name,
        &client,
        peer_has_memory_game,
    )
    .await;

    // Sync sliding puzzle scores
    crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
        db,
        peer_id,
        peer_url,
        display_name,
        &client,
        peer_has_sliding_puzzle,
    )
    .await;

    // Update peer's last_seen (and name if changed)
    if let Ok(Some(peer)) = crate::models::peer::Entity::find_by_id(peer_id)
        .one(db)
        .await
    {
        let mut active_peer: peer::ActiveModel = peer.into();
        if let Some(ref new_name) = updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        if let Some(ref avatar) = updated_avatar {
            active_peer.avatar_config = Set(Some(avatar.clone()));
        }
        active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active_peer.update(db).await;
    }

    tracing::info!(
        "✅ Background sync completed: {} books cached for peer {}",
        count,
        peer_id
    );
    Ok(count)
}

/// Sync gamification stats from a peer.
/// `peer_shares_stats`:
///   - `Some(true)`:  peer confirmed it shares → fetch fresh stats
///   - `Some(false)`: peer confirmed it does NOT share → delete cached stats
///   - `None`:        peer was unreachable (config unknown) → preserve cache, skip sync
pub(crate) async fn sync_peer_gamification_stats(
    db: &DatabaseConnection,
    peer_id: i32,
    peer_url: &str,
    client: &reqwest::Client,
    peer_shares_stats: Option<bool>,
) {
    use crate::models::installation_profile;

    // Check if network_gamification is enabled locally
    let local_enabled = match installation_profile::Entity::find_by_id(1).one(db).await {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"network_gamification".to_string())
        }
        _ => false,
    };

    if !local_enabled {
        return;
    }

    match peer_shares_stats {
        None => {
            // Peer unreachable — preserve cached data
            tracing::debug!(
                "Peer {} config unknown, preserving cached gamification stats",
                peer_url
            );
            return;
        }
        Some(false) => {
            // Peer explicitly does NOT share stats — clean up cache
            let _ = peer_gamification_stats::Entity::delete_many()
                .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
                .exec(db)
                .await;
            return;
        }
        Some(true) => {} // Peer shares — continue to fetch
    }

    // Fetch peer's public gamification stats
    let stats_url = format!("{}/api/gamification/public-stats", peer_url);
    let stats = match client.get(&stats_url).send().await {
        Ok(res) if res.status().is_success() => {
            match res
                .json::<crate::api::gamification::PublicGamificationStats>()
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to parse gamification stats from peer: {}", e);
                    return;
                }
            }
        }
        _ => {
            tracing::warn!("Failed to fetch gamification stats from peer {}", peer_url);
            return;
        }
    };

    // Upsert: delete old + insert new (same pattern as peer_books)
    let _ = peer_gamification_stats::Entity::delete_many()
        .filter(peer_gamification_stats::Column::PeerId.eq(peer_id))
        .exec(db)
        .await;

    let entry = peer_gamification_stats::ActiveModel {
        peer_id: Set(peer_id),
        library_name: Set(stats.library_name),
        collector_level: Set(stats.collector.level),
        collector_current: Set(stats.collector.current as i32),
        reader_level: Set(stats.reader.level),
        reader_current: Set(stats.reader.current as i32),
        lender_level: Set(stats.lender.level),
        lender_current: Set(stats.lender.current as i32),
        cataloguer_level: Set(stats.cataloguer.level),
        cataloguer_current: Set(stats.cataloguer.current as i32),
        synced_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    if let Err(e) = peer_gamification_stats::Entity::insert(entry)
        .exec(db)
        .await
    {
        tracing::warn!("Failed to save peer gamification stats: {}", e);
    } else {
        tracing::info!("Gamification stats synced for peer {}", peer_id);
    }
}

#[derive(Deserialize)]
pub struct IncomingConnectionRequest {
    name: String,
    url: String,
    /// Stable library UUID for P2P peer deduplication
    #[serde(default)]
    library_uuid: Option<String>,
    /// Ed25519 public key (hex) from the requesting peer - for E2EE
    #[serde(default)]
    ed25519_public_key: Option<String>,
    /// X25519 public key (hex) from the requesting peer - for E2EE
    #[serde(default)]
    x25519_public_key: Option<String>,
    /// Peer's relay hub URL
    #[serde(default)]
    relay_url: Option<String>,
    /// Peer's relay mailbox UUID
    #[serde(default)]
    mailbox_id: Option<String>,
    /// Token to write to peer's relay mailbox
    #[serde(default)]
    relay_write_token: Option<String>,
}

/// Receive an incoming connection request from a remote peer.
/// Always creates/updates the peer in local SQLite and returns our E2EE keys.
/// Also forwards to the Hub (fire-and-forget) for the central directory.
pub async fn receive_connection_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingConnectionRequest>,
) -> impl IntoResponse {
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
    // Find by URL first, then by library_uuid (handles port changes)
    let mut existing = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .one(&db)
        .await;

    if matches!(&existing, Ok(None))
        && let Some(ref uuid) = payload.library_uuid
    {
        existing = peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await;
    }

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
            // Always update name if the peer sent a non-empty one
            if !payload.name.is_empty() && payload.name != existing_peer.name {
                let _ = peer::Entity::update_many()
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
                    .await;
                tracing::info!(
                    "register_peer: updated peer {} name '{}' -> '{}'",
                    peer_id,
                    existing_peer.name,
                    payload.name
                );
            }

            if key_exchange_done && !existing_peer.key_exchange_done {
                let mut active: peer::ActiveModel = existing_peer.into();
                active.url = Set(payload.url.clone()); // Update URL (port may have changed)
                if payload.library_uuid.is_some() {
                    active.library_uuid = Set(payload.library_uuid.clone());
                }
                active.public_key = Set(payload.ed25519_public_key);
                active.x25519_public_key = Set(payload.x25519_public_key);
                active.key_exchange_done = Set(true);
                if payload.relay_url.is_some() {
                    active.relay_url = Set(payload.relay_url);
                }
                if payload.mailbox_id.is_some() {
                    active.mailbox_id = Set(payload.mailbox_id);
                }
                if payload.relay_write_token.is_some() {
                    active.relay_write_token = Set(payload.relay_write_token);
                    // ADR-032: fresh invitation clears any stale-token gate.
                    active.relay_write_token_invalid_at = Set(None);
                }
                active.updated_at = Set(Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
            }
            // If library_uuid changed (peer was reset), update it and clear cached books
            if let Some(new_uuid) = &payload.library_uuid
                && old_uuid.as_deref() != Some(new_uuid.as_str())
            {
                // Update library_uuid on the peer record
                let _ = peer::Entity::update_many()
                    .filter(peer::Column::Id.eq(peer_id))
                    .col_expr(
                        peer::Column::LibraryUuid,
                        sea_orm::sea_query::Expr::value(new_uuid.clone()),
                    )
                    .exec(&db)
                    .await;
                // Clear stale cached books if there was an old uuid
                if old_uuid.is_some() {
                    tracing::info!(
                        "register_peer: peer {} library_uuid changed, clearing cached books",
                        peer_id
                    );
                    let _ = peer_book::Entity::delete_many()
                        .filter(peer_book::Column::PeerId.eq(peer_id))
                        .exec(&db)
                        .await;
                }
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
            // Check if connection_validation module is enabled
            let connection_status = if is_connection_validation_enabled(&db).await {
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

pub async fn list_peers(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    // Legacy hub peer sync removed: peers are managed locally via invite
    // links, QR codes, and mDNS discovery. The old GET /api/peers hub
    // endpoint was causing SQLite lock contention and timeouts on every
    // list_peers call, making peers appear to vanish from the UI.

    let peers = peer::Entity::find().all(&db).await.unwrap_or(vec![]);

    // Convert to JSON with computed status field
    let peers_with_status: Vec<serde_json::Value> = peers
        .into_iter()
        .map(|p| {
            let status = if p.connection_status == "pending" {
                "pending"
            } else {
                "connected"
            };
            json!({
                "id": p.id,
                "name": p.name,
                "display_name": p.display_name,
                "url": p.url,
                "public_key": p.public_key,
                "library_uuid": p.library_uuid,
                "latitude": p.latitude,
                "longitude": p.longitude,
                "auto_approve": p.auto_approve,
                "connection_status": p.connection_status,
                "status": status,
                "relay_url": p.relay_url,
                "mailbox_id": p.mailbox_id,
                "relay_write_token": p.relay_write_token,
                "relay_write_token_invalid_at": p.relay_write_token_invalid_at,
                "last_seen": p.last_seen,
                "avatar_config": p.avatar_config,
                "created_at": p.created_at,
                "updated_at": p.updated_at,
            })
        })
        .collect();

    (
        StatusCode::OK,
        Json(json!({
            "data": peers_with_status
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct UpdatePeerStatusRequest {
    status: String, // "active" (accept) or "rejected"
}

/// Update a peer's status (accept or reject a connection request)
pub async fn update_peer_status(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerStatusRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // If rejecting, delete the peer entirely
    if payload.status == "rejected" {
        match peer::Entity::delete_by_id(peer_id).exec(&db).await {
            Ok(_) => {
                // Deactivate Library contacts associated with this peer (matched by name).
                use crate::models::contact;
                let _ = contact::Entity::update_many()
                    .filter(contact::Column::Name.eq(&peer.name))
                    .filter(contact::Column::Type.eq("Library"))
                    .col_expr(
                        contact::Column::IsActive,
                        sea_orm::sea_query::Expr::value(false),
                    )
                    .col_expr(
                        contact::Column::UpdatedAt,
                        sea_orm::sea_query::Expr::value(chrono::Utc::now().to_rfc3339()),
                    )
                    .exec(&db)
                    .await;
                tracing::info!("🗑️ Peer {} rejected and deleted", peer_id);
                return (
                    StatusCode::OK,
                    Json(json!({
                        "message": "Peer rejected and removed",
                        "peer_id": peer_id
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
                )
                    .into_response();
            }
        }
    }

    // Update auto_approve and connection_status for accept/active
    let auto_approve = payload.status == "active" || payload.status == "accepted";

    let mut active_model: peer::ActiveModel = peer.into();
    active_model.auto_approve = Set(auto_approve);
    if auto_approve {
        active_model.connection_status = Set("accepted".to_string());
    }
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active_model.update(&db).await {
        Ok(updated) => {
            tracing::info!("Peer {} accepted, auto_approve={}", peer_id, auto_approve);

            // Emit connection_accepted notification
            if auto_approve {
                crate::services::notification_service::emit(
                    &db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::ConnectionAccepted,
                        title: updated.name.clone(),
                        body: None,
                        ref_type: Some("peer".to_string()),
                        ref_id: Some(peer_id.to_string()),
                    },
                )
                .await;
            }

            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer accepted",
                    "peer": updated,
                    "auto_approve": auto_approve
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdatePeerUrlRequest {
    pub url: String,
    /// Optional library_uuid to backfill when discovered via mDNS.
    /// Validated as a proper UUID to prevent injection.
    pub library_uuid: Option<String>,
}

/// Update a peer's URL (for mDNS IP changes)
/// Security: Only pending peers can have their URL updated
pub async fn update_peer_url(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerUrlRequest>,
) -> impl IntoResponse {
    // Find the peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // Security: Only update URL for pending peers, unless upgrading from relay
    // to LAN or fixing a port mismatch (mDNS discovered the correct address).
    // This endpoint is localhost-only, so the caller is always the local app.
    if peer.auto_approve && !peer.url.starts_with("relay://") {
        // Allow port updates for same-host LAN URLs (hot restart changes port)
        let same_host = match (url::Url::parse(&peer.url), url::Url::parse(&payload.url)) {
            (Ok(old), Ok(new_url)) => old.host() == new_url.host(),
            _ => false,
        };
        if !same_host {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Cannot update URL for connected peers" })),
            )
                .into_response();
        }
    }

    // Check if URL is already taken by another peer
    if let Ok(Some(existing_peer)) = peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.url))
        .filter(peer::Column::Id.ne(peer_id))
        .one(&db)
        .await
    {
        // If the existing peer currently holding this URL is pending (not auto_approve),
        // we can assume it's a stale entry (e.g. from a previous mDNS discovery on this IP)
        // and delete it to free up the URL.
        if !existing_peer.auto_approve {
            tracing::info!(
                "♻️ deleting stale peer {} to free up URL {}",
                existing_peer.id,
                payload.url
            );
            let _ = peer::Entity::delete_by_id(existing_peer.id).exec(&db).await;
        } else {
            // If it's an approved peer, we can't just delete it.
            // This is a genuine conflict (two trusted peers on same IP? or same peer different ID?)
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "URL already in use by another trusted peer" })),
            )
                .into_response();
        }
    }

    let mut active_model: peer::ActiveModel = peer.into();
    active_model.url = Set(payload.url.clone());
    active_model.updated_at = Set(chrono::Utc::now().to_rfc3339());

    // Backfill library_uuid if provided and valid UUID format
    if let Some(ref uuid_str) = payload.library_uuid {
        if uuid::Uuid::parse_str(uuid_str).is_ok() {
            active_model.library_uuid = Set(Some(uuid_str.clone()));
            tracing::info!(
                "Backfilling library_uuid for peer {}: {}",
                peer_id,
                uuid_str
            );
        } else {
            tracing::warn!(
                "Ignoring invalid library_uuid for peer {}: {}",
                peer_id,
                uuid_str
            );
        }
    }

    match active_model.update(&db).await {
        Ok(updated) => {
            tracing::info!("✅ Peer {} URL updated to: {}", peer_id, payload.url);
            (
                StatusCode::OK,
                Json(json!({
                    "message": "Peer URL updated",
                    "peer": updated
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update peer: {}", e) })),
        )
            .into_response(),
    }
}

/// Removes cached hub directory catalog entries (peer_id = 0 sentinel) for a
/// given library_uuid. See ADR-024: the cache is owned by the peer relationship,
/// so deletion must invalidate it to prevent stale reads on re-add.
async fn purge_hub_catalog_cache(db: &DatabaseConnection, library_uuid: &str) {
    use crate::models::peer_book;
    match peer_book::Entity::delete_many()
        .filter(peer_book::Column::NodeId.eq(library_uuid))
        .filter(peer_book::Column::PeerId.eq(0))
        .exec(db)
        .await
    {
        Ok(res) => tracing::info!(
            "Purged {} hub catalog cache entries for library_uuid={}",
            res.rows_affected,
            library_uuid
        ),
        Err(e) => tracing::warn!(
            "Failed to purge hub catalog cache for library_uuid={}: {}",
            library_uuid,
            e
        ),
    }
}

pub async fn delete_peer(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Load peer before deletion so we can notify the remote side
    let peer_model = match peer::Entity::find_by_id(peer_id).one(db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Notify remote peer (fire-and-forget, never blocks local deletion)
    let state_clone = state.clone();
    let peer_clone = peer_model.clone();
    tokio::spawn(async move {
        notify_peer_of_disconnect(&state_clone, &peer_clone).await;
    });

    // 3. Delete locally
    match peer::Entity::delete_by_id(peer_id).exec(db).await {
        Ok(_) => {
            // Deactivate Library contacts associated with this peer (matched by name).
            // These contacts were auto-created during P2P interactions and are stale
            // now that the peer connection is gone.
            use crate::models::contact;
            let _ = contact::Entity::update_many()
                .filter(contact::Column::Name.eq(&peer_model.name))
                .filter(contact::Column::Type.eq("Library"))
                .col_expr(
                    contact::Column::IsActive,
                    sea_orm::sea_query::Expr::value(false),
                )
                .col_expr(
                    contact::Column::UpdatedAt,
                    sea_orm::sea_query::Expr::value(chrono::Utc::now().to_rfc3339()),
                )
                .exec(db)
                .await;
            // ADR-024: purge the hub directory catalog cache for this peer's
            // library_uuid so re-adding the same peer does not serve stale entries.
            if let Some(ref uuid) = peer_model.library_uuid {
                purge_hub_catalog_cache(db, uuid).await;
            }
            tracing::info!("🗑️ Peer {} ({}) deleted", peer_id, peer_model.name);
            (StatusCode::OK, Json(json!({ "message": "Peer deleted" }))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to delete peer: {}", e) })),
        )
            .into_response(),
    }
}

/// Notify a remote peer that we are disconnecting.
///
/// Tries E2EE first (encrypted, with relay fallback for offline peers),
/// then falls back to a plaintext HTTP POST for peers without E2EE keys.
/// Errors are logged but never propagated - disconnection is always local-first.
async fn notify_peer_of_disconnect(
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
    match peer::Entity::delete_by_id(peer_id).exec(db).await {
        Ok(_) => {
            info!(
                "Peer {} ({}) removed via authenticated disconnect (uuid={})",
                peer_name, peer_id, library_uuid
            );
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

#[derive(Deserialize)]
pub struct PushRequest {
    operations: Vec<OperationDto>,
}

#[derive(Serialize, Deserialize)]
pub struct OperationDto {
    entity_type: String,
    entity_id: i32,
    operation: String,
    payload: Option<String>,
    created_at: String,
}

pub async fn push_operations(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<PushRequest>,
) -> impl IntoResponse {
    // Simplified: just log them for now, in real app we'd apply them
    for op in payload.operations {
        let log = operation_log::ActiveModel {
            entity_type: Set(op.entity_type),
            entity_id: Set(op.entity_id),
            operation: Set(op.operation),
            payload: Set(op.payload),
            created_at: Set(op.created_at),
            ..Default::default()
        };
        let _ = operation_log::Entity::insert(log).exec(&db).await;
    }
    (
        StatusCode::OK,
        Json(json!({ "message": "Operations received" })),
    )
        .into_response()
}

pub async fn pull_operations(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    let ops = operation_log::Entity::find()
        .all(&db)
        .await
        .unwrap_or(vec![]);
    (StatusCode::OK, Json(ops)).into_response()
}

#[derive(Deserialize)]
pub struct SearchRequest {
    query: String,
}

pub async fn search_local(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<SearchRequest>,
) -> impl IntoResponse {
    use crate::models::book;
    use sea_orm::sea_query::Expr;

    let books = book::Entity::find()
        .filter(book::Column::Private.eq(false))
        .filter(
            Condition::any()
                .add(book::Column::Title.contains(&payload.query))
                .add(
                    Expr::col(book::Column::Id)
                        .in_subquery(crate::models::Book::author_search_subquery(&payload.query)),
                ),
        )
        .all(&db)
        .await
        .unwrap_or(vec![]);

    let mut book_dtos = crate::models::Book::populate_authors(&db, books).await;
    crate::models::Book::rewrite_local_cover_urls(&mut book_dtos, None);
    (StatusCode::OK, Json(book_dtos)).into_response()
}

#[derive(Deserialize)]
pub struct ProxySearchRequest {
    peer_id: Option<i32>,
    peer_url: Option<String>,
    query: String,
    page: Option<u64>,
    limit: Option<u64>,
}

/// Plaintext HTTP proxy: fetch books from a peer URL directly.
/// When `page`/`limit` are provided, returns `{ "books": [...], "total": N, "has_more": bool }`.
/// Without pagination params, returns a flat `Vec<Book>` array (legacy).
/// The peer's response carries `added_at` directly (the owner's
/// `books.created_at`), so the "new" badge works without local enrichment.
async fn plaintext_proxy_search(
    peer_url: &str,
    query: &str,
    page: Option<u64>,
    limit: Option<u64>,
) -> axum::response::Response {
    let client = get_safe_client();
    let res = if query.is_empty() {
        let mut url = format!("{}/api/books?owned_only=true", peer_url);
        if let Some(p) = page {
            let l = limit.unwrap_or(20).min(50);
            url.push_str(&format!("&page={}&limit={}", p, l));
        }
        client.get(&url).send().await
    } else {
        let url = format!("{}/api/peers/search", peer_url);
        client
            .post(&url)
            .json(&json!({ "query": query }))
            .send()
            .await
    };

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // /api/books returns {"books": [...], "total": N}
                // /api/peers/search returns [...]
                let body: serde_json::Value = response.json().await.unwrap_or(json!([]));

                if page.is_some() && query.is_empty() {
                    // Paginated: return envelope with has_more
                    let books: Vec<crate::models::Book> = body
                        .get("books")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let total = body.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                    let p = page.unwrap_or(0);
                    let l = limit.unwrap_or(20).min(50);
                    let has_more = ((p + 1) * l) < total;
                    (
                        StatusCode::OK,
                        Json(json!({
                            "books": books,
                            "total": total,
                            "has_more": has_more,
                        })),
                    )
                        .into_response()
                } else {
                    // Legacy: return flat array
                    let books: Vec<crate::models::Book> = if let Some(arr) = body.get("books") {
                        serde_json::from_value(arr.clone()).unwrap_or_default()
                    } else {
                        serde_json::from_value(body).unwrap_or_default()
                    };
                    (StatusCode::OK, Json(books)).into_response()
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned an error" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

pub async fn proxy_search(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<ProxySearchRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find peer by id or url
    let peer = if let Some(id) = payload.peer_id {
        peer::Entity::find_by_id(id).one(db).await.unwrap_or(None)
    } else if let Some(ref url) = payload.peer_url {
        peer::Entity::find()
            .filter(peer::Column::Url.eq(url.as_str()))
            .one(db)
            .await
            .unwrap_or(None)
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer_id or peer_url required" })),
        )
            .into_response();
    };

    if let Some(peer) = peer {
        // Validate Peer URL (just in case it was modified in DB)
        if let Err(e) = validate_url(&peer.url) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }

        // Paginated library browse via E2EE (empty query + page param)
        if payload.query.is_empty() && payload.page.is_some() {
            let page = payload.page.unwrap_or(0);
            let limit = payload.limit.unwrap_or(20).min(50);
            match try_send_e2ee(
                &state,
                &peer,
                "library_browse_request",
                json!({ "page": page, "limit": limit }),
            )
            .await
            {
                Ok(Some(Some(response_msg))) => {
                    return (StatusCode::OK, Json(response_msg.payload)).into_response();
                }
                Ok(Some(None)) | Ok(None) | Err(_) => {
                    return plaintext_proxy_search(
                        &peer.url,
                        &payload.query,
                        payload.page,
                        payload.limit,
                    )
                    .await;
                }
            }
        }

        // Try E2EE path first (search is request-response: returns encrypted results)
        match try_send_e2ee(
            &state,
            &peer,
            "search_request",
            json!({ "query": payload.query }),
        )
        .await
        {
            Ok(Some(Some(response_msg))) => {
                // Got encrypted search results
                let results: Vec<crate::models::Book> = serde_json::from_value(
                    response_msg
                        .payload
                        .get("results")
                        .cloned()
                        .unwrap_or(json!([])),
                )
                .unwrap_or_default();
                return (StatusCode::OK, Json(results)).into_response();
            }
            Ok(Some(None)) => {
                // E2EE sent but no response body (unexpected for search)
                return (StatusCode::OK, Json(Vec::<crate::models::Book>::new())).into_response();
            }
            Ok(None) => {} // Fallback to plaintext
            Err(e) => {
                tracing::warn!("E2EE proxy_search failed, falling back to plaintext: {}", e);
            }
        }

        // 2. Legacy plaintext fallback
        return plaintext_proxy_search(&peer.url, &payload.query, payload.page, payload.limit)
            .await;
    }

    // Peer not in DB but URL provided (e.g. unsaved mDNS peer): direct plaintext fetch.
    // SSRF defense (ADR-026): route through ensure_registered_peer_or_mdns with
    // allow_unregistered_lan=true so the traversal is logged on the ssrf:mdns
    // tracing target. ensure_* also reconciles the trailing-slash discrepancy
    // with the peer lookup above, so a DB hit via helper still routes through
    // the enriched path instead of the unsaved branch.
    if let Some(ref url) = payload.peer_url {
        if let Err(e) = validate_url(url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }
        match ensure_registered_peer_or_mdns(db, url, true).await {
            Ok(Some(matched)) => {
                return plaintext_proxy_search(
                    &matched.url,
                    &payload.query,
                    payload.page,
                    payload.limit,
                )
                .await;
            }
            Ok(None) => {
                return plaintext_proxy_search(url, &payload.query, payload.page, payload.limit)
                    .await;
            }
            Err(status) => return status.into_response(),
        }
    }

    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "Peer not found" })),
    )
        .into_response()
}

pub async fn sync_peer(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    // Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // 2. Validate URL and fetch remote books
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();

    // Check peer config for gamification sharing
    let config_url = format!("{}/api/config", peer.url);
    let peer_config = match client.get(&config_url).send().await {
        Ok(res) if res.status().is_success() => {
            res.json::<crate::api::setup::ConfigResponse>().await.ok()
        }
        _ => None,
    };
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));
    let peer_display_name = peer_config
        .as_ref()
        .map(|c| c.library_name.clone())
        .unwrap_or_else(|| peer.name.clone());

    // Update library_uuid: backfill if missing, or detect changes (peer reset).
    // Validates UUID format to prevent a malicious peer from injecting arbitrary strings.
    if let Some(remote_uuid) = peer_config.as_ref().and_then(|c| c.library_uuid.clone()) {
        if uuid::Uuid::parse_str(&remote_uuid).is_ok() {
            let uuid_changed = peer
                .library_uuid
                .as_ref()
                .is_some_and(|old| old != &remote_uuid);
            let uuid_missing = peer.library_uuid.is_none();

            if uuid_changed || uuid_missing {
                let mut active: peer::ActiveModel = peer.clone().into();
                active.library_uuid = Set(Some(remote_uuid.clone()));
                if let Err(e) = active.update(&db).await {
                    tracing::warn!("Failed to update library_uuid for peer {}: {}", peer_id, e);
                } else if uuid_changed {
                    // Peer was reset/reinstalled - upsert_peer_books_cache will
                    // handle the transition atomically (insert new, update
                    // existing, delete absent). No premature cache wipe needed.
                    tracing::info!(
                        "Peer {} library_uuid changed during sync, will refresh via upsert",
                        peer_id
                    );
                } else {
                    tracing::info!("Backfilled library_uuid for peer {}", peer_id);
                }
            }
        } else {
            tracing::warn!("Peer {} sent invalid library_uuid, ignoring", peer_id);
        }
    }

    let url = format!("{}/api/books?owned_only=true", peer.url);

    let res = client.get(&url).send().await;

    match res {
        Ok(response) => {
            if response.status().is_success() {
                // Parse response: { "books": [...] }
                #[derive(Deserialize)]
                struct BooksResponse {
                    books: Vec<crate::models::Book>,
                }

                match response.json::<BooksResponse>().await {
                    Ok(data) => {
                        // Upsert books cache (preserves first_seen_at)
                        let count = upsert_peer_books_cache(&db, peer.id, None, data.books).await;

                        // Sync gamification stats
                        sync_peer_gamification_stats(
                            &db,
                            peer.id,
                            &peer.url,
                            &client,
                            shares_gamification,
                        )
                        .await;

                        // Sync memory game scores
                        crate::modules::memory_game::handlers::sync_peer_memory_scores(
                            &db,
                            peer.id,
                            &peer.url,
                            &peer_display_name,
                            &client,
                            peer_has_memory_game,
                        )
                        .await;

                        // Sync sliding puzzle scores
                        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
                            &db,
                            peer.id,
                            &peer.url,
                            &peer_display_name,
                            &client,
                            peer_has_sliding_puzzle,
                        )
                        .await;

                        (
                            StatusCode::OK,
                            Json(json!({ "message": "Sync successful", "count": count })),
                        )
                            .into_response()
                    }
                    _ => (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "Invalid response format" })),
                    )
                        .into_response(),
                }
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer returned error" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

/// Sync peer by URL (solves ID mismatch between Hub and Backend)
pub async fn sync_peer_by_url(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let db = state.db().clone();

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // 1. Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            // Peer not found locally, try to fetch from Hub
            let mut found_peer = None;

            if let Ok(hub_url) = std::env::var("HUB_URL") {
                let client = get_safe_client();
                let url = format!("{}/api/peers", hub_url);

                if let Ok(res) = client.get(&url).send().await
                    && res.status().is_success()
                {
                    #[derive(Deserialize)]
                    struct HubPeer {
                        name: String,
                        url: String,
                        #[serde(rename = "status")]
                        _status: String,
                    }
                    #[derive(Deserialize)]
                    struct HubResponse {
                        data: Vec<HubPeer>,
                    }

                    if let Ok(hub_res) = res.json::<HubResponse>().await {
                        for hub_peer in hub_res.data {
                            let hub_docker_url = translate_url_for_docker(&hub_peer.url);

                            // Match by URL
                            if hub_docker_url == docker_url {
                                // Insert new peer
                                let new_peer = peer::ActiveModel {
                                    name: Set(hub_peer.name),
                                    url: Set(hub_docker_url.clone()),
                                    created_at: Set(chrono::Utc::now().to_rfc3339()),
                                    updated_at: Set(chrono::Utc::now().to_rfc3339()),
                                    ..Default::default()
                                };

                                if let Ok(res) = peer::Entity::insert(new_peer).exec(&db).await {
                                    // Fetch the inserted peer to return it
                                    found_peer = peer::Entity::find_by_id(res.last_insert_id)
                                        .one(&db)
                                        .await
                                        .unwrap_or(None);
                                }
                                break;
                            }
                        }
                    }
                }
            }

            match found_peer {
                Some(p) => p,
                None => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(
                            json!({ "error": format!("Peer not found with URL: {}", docker_url) }),
                        ),
                    )
                        .into_response();
                }
            }
        }
    };

    // 2. Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // 3. Validate URL
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
        )
            .into_response();
    }

    let client = get_safe_client();

    // 4. Check peer's config for privacy consent flags.
    // Skip the network fetch + port scan if the peer is cached as unreachable --
    // avoids burning 5-10s on timeouts we already know will fail.
    let peer_cached_unreachable = state.is_peer_direct_unreachable(peer.id);

    let mut peer_config = if peer_cached_unreachable {
        tracing::debug!(
            "Sync: Skipping config fetch for peer {} (cached unreachable)",
            peer.name,
        );
        None
    } else {
        let config_url = format!("{}/api/config", peer.url);
        match client.get(&config_url).send().await {
            Ok(res) if res.status().is_success() => {
                res.json::<crate::api::setup::ConfigResponse>().await.ok()
            }
            _ => None,
        }
    };

    // 4b. If config fetch failed, the peer may have restarted on a different port.
    // Try scanning ports 8000-8010 on the same host.
    // (Skip when peer is cached unreachable -- port scan would also timeout.)
    let effective_url = if peer_config.is_none() && !peer_cached_unreachable {
        match crate::utils::peer_discovery::try_discover_peer_port(&peer.url, &client).await {
            Some(new_url) => {
                // Retry config fetch with discovered URL
                let retry_url = format!("{}/api/config", new_url);
                peer_config = match client.get(&retry_url).send().await {
                    Ok(res) if res.status().is_success() => {
                        res.json::<crate::api::setup::ConfigResponse>().await.ok()
                    }
                    _ => None,
                };
                new_url
            }
            None => peer.url.clone(),
        }
    } else {
        peer.url.clone()
    };

    // Distinguish "peer explicitly disallows caching" from "peer unreachable"
    // When peer_config is None (unreachable on 5G), preserve cache and try E2EE/relay
    let peer_reachable = peer_config.is_some();
    let allows_caching = peer_config
        .as_ref()
        .map(|c| c.allow_library_caching)
        .unwrap_or(true); // assume caching OK when unreachable - preserve cache
    let shares_gamification = peer_config.as_ref().map(|c| c.share_gamification_stats);
    let peer_has_memory_game_url = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"memory_game".to_string()));
    let peer_has_sliding_puzzle_url = peer_config
        .as_ref()
        .map(|c| c.enabled_modules.contains(&"sliding_puzzle".to_string()));
    let peer_display_name_url = peer_config
        .as_ref()
        .map(|c| c.library_name.clone())
        .unwrap_or_else(|| peer.name.clone());

    // Extract updated name from peer config (if changed)
    let updated_name = peer_config
        .as_ref()
        .filter(|c| c.library_name != peer.name)
        .map(|c| c.library_name.clone());

    // Extract updated avatar config from peer config (if changed)
    let updated_avatar = peer_config.as_ref().and_then(|c| {
        let new_json = c
            .avatar_config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        tracing::info!(
            "Sync avatar check for peer {}: remote={:?}, stored={:?}",
            peer.name,
            new_json.as_deref().map(|s| &s[..s.len().min(50)]),
            peer.avatar_config.as_deref().map(|s| &s[..s.len().min(50)]),
        );
        if new_json != peer.avatar_config {
            new_json
        } else {
            None
        }
    });

    // Refresh relay credentials from peer config if they changed
    if let Some(ref config) = peer_config {
        let new_relay = (
            config.relay_url.as_deref(),
            config.mailbox_id.as_deref(),
            config.relay_write_token.as_deref(),
        );
        let old_relay = (
            peer.relay_url.as_deref(),
            peer.mailbox_id.as_deref(),
            peer.relay_write_token.as_deref(),
        );
        if new_relay != old_relay
            && let (Some(r_url), Some(m_id), Some(w_tok)) = new_relay
            && !r_url.is_empty()
            && !m_id.is_empty()
            && !w_tok.is_empty()
            && let Ok(Some(existing)) = peer::Entity::find_by_id(peer.id).one(&db).await
        {
            let mut active: peer::ActiveModel = existing.into();
            active.relay_url = Set(Some(r_url.to_string()));
            active.mailbox_id = Set(Some(m_id.to_string()));
            active.relay_write_token = Set(Some(w_tok.to_string()));
            // ADR-032: fresh credentials from the peer's /api/config clear
            // any stale-token gate.
            active.relay_write_token_invalid_at = Set(None);
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(&db).await;
            tracing::info!(
                "Sync: Updated relay credentials for peer {} (mailbox: {})",
                peer.name,
                m_id
            );
        }
    }

    if peer_reachable && !allows_caching {
        // Peer is reachable and explicitly disallows caching - clear cache
        let _ = peer_book::Entity::delete_many()
            .filter(peer_book::Column::PeerId.eq(peer.id))
            .exec(&db)
            .await;
        // Peer doesn't allow caching - still sync gamification stats
        sync_peer_gamification_stats(&db, peer.id, &effective_url, &client, shares_gamification)
            .await;
        // Still sync memory game scores
        crate::modules::memory_game::handlers::sync_peer_memory_scores(
            &db,
            peer.id,
            &effective_url,
            &peer_display_name_url,
            &client,
            peer_has_memory_game_url,
        )
        .await;
        // Still sync sliding puzzle scores
        crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
            &db,
            peer.id,
            &effective_url,
            &peer_display_name_url,
            &client,
            peer_has_sliding_puzzle_url,
        )
        .await;

        let peer_id = peer.id;
        let url_changed = effective_url != peer.url;
        // Re-read peer from DB to avoid overwriting concurrent changes
        if let Ok(Some(fresh_peer)) = peer::Entity::find_by_id(peer_id).one(&db).await {
            let mut active_peer: peer::ActiveModel = fresh_peer.into();
            if url_changed {
                active_peer.url = Set(effective_url);
                tracing::info!("Port discovery: persisted new URL for peer {}", peer_id);
            }
            if let Some(ref new_name) = updated_name {
                active_peer.name = Set(new_name.clone());
                tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
            }
            if let Some(ref avatar) = updated_avatar {
                active_peer.avatar_config = Set(Some(avatar.clone()));
            }
            active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
            active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active_peer.update(&db).await;
        }

        return (
            StatusCode::OK,
            Json(json!({
                "message": "Peer does not allow library caching",
                "count": 0,
                "peer_id": peer_id,
                "caching_allowed": false
            })),
        )
            .into_response();
    }

    // 4. Fetch remote books — try E2EE first, then plaintext fallback
    // When the peer is unreachable via direct HTTP (e.g. on 5G), avatar_config and
    // library_name are also piggy-backed on the E2EE response as a fallback.
    //
    // Diff-based: send the catalog_hash we cached on the peer row last time.
    // The responder (handle_book_sync_request) returns a tiny "unchanged"
    // payload when its current hash matches, saving the ~95 KB book list on
    // every uneventful poll.
    let mut e2ee_avatar: Option<String> = None;
    let mut e2ee_library_name: Option<String> = None;
    let mut sync_unchanged: bool = false;
    let mut new_catalog_hash: Option<String> = None;
    let cached_catalog_hash = peer.catalog_hash.clone();
    let request_payload = match cached_catalog_hash.as_deref() {
        Some(h) => json!({ "catalog_hash": h }),
        None => json!({}),
    };
    let books: Vec<crate::models::Book> =
        match try_send_e2ee(&state, &peer, "book_sync_request", request_payload).await {
            Ok(Some(Some(response_msg))) => {
                // Extract avatar and library name for relay-only sync (5G fallback)
                e2ee_avatar = response_msg
                    .payload
                    .get("avatar_config")
                    .and_then(|v| serde_json::to_string(v).ok())
                    .filter(|s| s != "null" && !s.is_empty());
                e2ee_library_name = response_msg
                    .payload
                    .get("library_name")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));
                new_catalog_hash = response_msg
                    .payload
                    .get("catalog_hash")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));

                let status = response_msg.payload.get("status").and_then(|v| v.as_str());
                if status == Some("unchanged") {
                    // Catalog unchanged: keep the previous local cache
                    // entries intact and skip the (now redundant) upsert.
                    sync_unchanged = true;
                    Vec::new()
                } else {
                    // Got encrypted book list
                    serde_json::from_value(
                        response_msg
                            .payload
                            .get("books")
                            .cloned()
                            .unwrap_or(json!([])),
                    )
                    .unwrap_or_default()
                }
            }
            Ok(Some(None)) => {
                // E2EE sent but no response body (unexpected for sync)
                vec![]
            }
            Ok(None) | Err(_) => {
                // Fallback to plaintext
                let url = format!("{}/api/books?owned_only=true", effective_url);
                match client.get(&url).send().await {
                    Ok(response) if response.status().is_success() => {
                        #[derive(Deserialize)]
                        struct BooksResponse {
                            books: Vec<crate::models::Book>,
                        }
                        response
                            .json::<BooksResponse>()
                            .await
                            .map(|d| d.books)
                            .unwrap_or_default()
                    }
                    Ok(_) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": "Peer returned error" })),
                        )
                            .into_response();
                    }
                    Err(_) => {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": "Failed to contact peer" })),
                        )
                            .into_response();
                    }
                }
            }
        };

    // 5. Upsert books cache (preserves first_seen_at).
    // Skip when the responder reported "unchanged": the cache already has
    // the correct entries from the previous successful sync, and re-running
    // the upsert with an empty `books` would mistakenly delete them.
    let count = if sync_unchanged {
        crate::models::peer_book::Entity::find()
            .filter(crate::models::peer_book::Column::PeerId.eq(peer.id))
            .count(&db)
            .await
            .unwrap_or(0) as usize
    } else {
        upsert_peer_books_cache(&db, peer.id, None, books).await
    };

    // 6. Sync gamification stats
    sync_peer_gamification_stats(&db, peer.id, &effective_url, &client, shares_gamification).await;

    // 6b. Sync memory game scores
    crate::modules::memory_game::handlers::sync_peer_memory_scores(
        &db,
        peer.id,
        &effective_url,
        &peer_display_name_url,
        &client,
        peer_has_memory_game_url,
    )
    .await;

    // 6c. Sync sliding puzzle scores
    crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
        &db,
        peer.id,
        &effective_url,
        &peer_display_name_url,
        &client,
        peer_has_sliding_puzzle_url,
    )
    .await;

    // 7. Update peer's last_seen (and name/URL/avatar if changed)
    // Re-read the peer from DB to avoid overwriting concurrent changes
    // (the `peer` variable was loaded at the start of this function and is stale).
    let peer_id = peer.id;
    let url_changed = effective_url != peer.url;
    // Fall back to E2EE-sourced metadata when peer was unreachable via direct HTTP (5G)
    let final_updated_name = updated_name.or_else(|| e2ee_library_name.filter(|n| n != &peer.name));
    let final_updated_avatar = updated_avatar
        .or_else(|| e2ee_avatar.filter(|a| Some(a.as_str()) != peer.avatar_config.as_deref()));
    if let Ok(Some(fresh_peer)) = peer::Entity::find_by_id(peer_id).one(&db).await {
        let mut active_peer: peer::ActiveModel = fresh_peer.into();
        if url_changed {
            active_peer.url = Set(effective_url);
            tracing::info!("Port discovery: persisted new URL for peer {}", peer_id);
        }
        if let Some(ref new_name) = final_updated_name {
            active_peer.name = Set(new_name.clone());
            tracing::info!("Updated peer {} name to '{}'", peer_id, new_name);
        }
        if let Some(ref avatar) = final_updated_avatar {
            active_peer.avatar_config = Set(Some(avatar.clone()));
        }
        // Persist the catalog hash returned by the responder so the next
        // book_sync_request can short-circuit to "unchanged" when the peer
        // catalog is still the same. Only update when we actually got a
        // hash back (avoid clobbering with None on transport errors).
        if let Some(ref hash) = new_catalog_hash {
            active_peer.catalog_hash = Set(Some(hash.clone()));
            active_peer.last_catalog_sync = Set(Some(chrono::Utc::now().to_rfc3339()));
        }
        active_peer.last_seen = Set(Some(chrono::Utc::now().to_rfc3339()));
        active_peer.updated_at = Set(chrono::Utc::now().to_rfc3339());
        let _ = active_peer.update(&db).await;
    }

    (
        StatusCode::OK,
        Json(json!({ "message": "Sync successful", "count": count, "peer_id": peer_id })),
    )
        .into_response()
}

// --- Federated Search Helper ---

pub async fn broadcast_search(
    db: &DatabaseConnection,
    params: &crate::api::search::SearchQuery,
) -> Vec<crate::models::Book> {
    let peers = peer::Entity::find().all(db).await.unwrap_or(vec![]);
    if peers.is_empty() {
        return vec![];
    }

    let client = get_safe_client();
    let query_str = params.title.clone().unwrap_or_default(); // Simple query for now

    let futures = peers.into_iter().map(|peer| {
        let client = client.clone();
        let q = query_str.clone();
        async move {
            if validate_url(&peer.url).is_err() {
                return vec![];
            }
            let url = format!("{}/api/peers/search", peer.url);
            match client
                .post(&url)
                .json(&json!({ "query": q }))
                .timeout(std::time::Duration::from_secs(2)) // 2s timeout
                .send()
                .await
            {
                Ok(res) => {
                    match res.json::<Vec<crate::models::Book>>().await {
                        Ok(mut books) => {
                            // Tag source and embed peer_id for request
                            for b in &mut books {
                                b.source = Some(format!("Peer: {}", peer.name));
                                // Hack: Embed peer_id in source_data so frontend can use it
                                b.source_data = Some(json!({ "peer_id": peer.id }).to_string());
                            }
                            books
                        }
                        _ => {
                            vec![]
                        }
                    }
                }
                Err(_) => vec![],
            }
        }
    });

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}

/// Borrower-side: process an auto-approve acceptance from the lender's synchronous response.
///
/// Updates the outgoing request to "accepted" and creates a borrowed copy in the local library.
/// Called from both E2EE and plaintext paths when the lender auto-accepts.
async fn process_borrower_acceptance(
    db: &DatabaseConnection,
    outgoing_id: &str,
    payload: &serde_json::Value,
    lender_request_id: Option<&str>,
) {
    use crate::models::{book, copy, p2p_outgoing_request};

    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let isbn = payload.get("isbn").and_then(|v| v.as_str());
    let cover_url = payload
        .get("cover_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let lender_name = payload
        .get("lender_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let due_date = payload
        .get("due_date")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");

    if title.is_empty() {
        tracing::warn!("process_borrower_acceptance: empty title, skipping");
        return;
    }

    // 1. Update outgoing request to "accepted"
    if let Ok(Some(outgoing)) = p2p_outgoing_request::Entity::find_by_id(outgoing_id)
        .one(db)
        .await
    {
        let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
        active.status = Set("accepted".to_string());
        if let Some(lr_id) = lender_request_id {
            active.lender_request_id = Set(Some(lr_id.to_string()));
        }
        active.updated_at = Set(Utc::now().to_rfc3339());
        let _ = active.update(db).await;
    }

    // 2. Find or create book
    let existing_book = if let Some(isbn_val) = isbn
        && !isbn_val.is_empty()
    {
        book::Entity::find()
            .filter(book::Column::Isbn.eq(isbn_val))
            .one(db)
            .await
            .ok()
            .flatten()
    } else {
        book::Entity::find()
            .filter(book::Column::Title.eq(title))
            .one(db)
            .await
            .ok()
            .flatten()
    };

    let book_id = match existing_book {
        Some(b) => b.id,
        None => {
            let now = Utc::now().to_rfc3339();
            let new_book = book::ActiveModel {
                title: Set(title.to_string()),
                isbn: Set(isbn.map(|s| s.to_string())),
                cover_url: Set(cover_url.clone()),
                owned: Set(false),
                created_at: Set(now.clone()),
                updated_at: Set(now),
                ..Default::default()
            };
            match new_book.insert(db).await {
                Ok(b) => b.id,
                Err(e) => {
                    tracing::error!("process_borrower_acceptance: failed to create book: {e}");
                    return;
                }
            }
        }
    };

    // 3. Idempotency: skip if a borrowed temporary copy already exists
    let existing_borrowed = copy::Entity::find()
        .filter(copy::Column::BookId.eq(book_id))
        .filter(copy::Column::Status.eq("borrowed"))
        .filter(copy::Column::IsTemporary.eq(true))
        .one(db)
        .await
        .ok()
        .flatten();

    if existing_borrowed.is_some() {
        tracing::info!(
            "process_borrower_acceptance: borrowed copy already exists for book_id={}",
            book_id
        );
        return;
    }

    // 4. Create borrowed copy
    let lib_id = match crate::utils::library_helpers::resolve_library_id(db).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("process_borrower_acceptance: failed to resolve library: {e}");
            return;
        }
    };
    let now = Utc::now().to_rfc3339();
    let new_copy = copy::ActiveModel {
        book_id: Set(book_id),
        library_id: Set(lib_id),
        status: Set("borrowed".to_string()),
        is_temporary: Set(true),
        notes: Set(Some(format!(
            "Emprunté de {lender_name} jusqu'au {due_date}"
        ))),
        acquisition_date: Set(Some(now.clone())),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_copy.insert(db).await {
        Ok(c) => {
            tracing::info!(
                "process_borrower_acceptance: created borrowed copy id={} for book_id={}",
                c.id,
                book_id
            );
            // Notify the borrower that the loan was accepted
            crate::services::notification_service::emit(
                db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowAccepted,
                    title: title.to_string(),
                    body: Some(lender_name.to_string()),
                    ref_type: Some("peer".to_string()),
                    ref_id: None,
                },
            )
            .await;
        }
        Err(e) => {
            tracing::error!("process_borrower_acceptance: failed to create copy: {e}");
        }
    }
}

#[derive(Deserialize)]
pub struct BookRequest {
    book_isbn: String,
    book_title: String,
}

pub async fn request_book(
    State(state): State<crate::infrastructure::AppState>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<BookRequest>,
) -> impl IntoResponse {
    let db = state.db();

    // 1. Find peer
    let peer = match peer::Entity::find_by_id(peer_id).one(db).await {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    // 2. Guards: prevent invalid borrow requests.
    {
        use crate::models::{p2p_outgoing_request, p2p_request};

        // 2a. Reject if there is already a pending or accepted outgoing request for this
        //     book from this peer (prevents double-borrowing the same copy).
        let already_borrowing = p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::ToPeerId.eq(peer.id))
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(
                Condition::any()
                    .add(p2p_outgoing_request::Column::Status.eq("pending"))
                    .add(p2p_outgoing_request::Column::Status.eq("accepted")),
            )
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        // 2b. Reject if user is currently lending this book to the same peer
        //     (prevents borrowing back a book that is out on loan to them).
        let currently_lending = p2p_request::Entity::find()
            .filter(p2p_request::Column::FromPeerId.eq(peer.id))
            .filter(p2p_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(p2p_request::Column::Status.eq("accepted"))
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        if already_borrowing || currently_lending {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "already_requested" })),
            )
                .into_response();
        }
    }

    // 3. Save Outgoing Request
    let outgoing_id = uuid::Uuid::new_v4().to_string();
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(outgoing_id.clone()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        lender_request_id: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(db)
        .await
    {
        tracing::error!("❌ Failed to save outgoing status: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 4. Send request to peer
    // Note: validate_url is deferred to the plaintext fallback path below.
    // Relay-only peers have a relay:// URL that is valid for E2EE but not for
    // direct HTTP, so SSRF validation must not block the E2EE path.

    // Try E2EE path first
    let my_config = match crate::models::library_config::Entity::find().one(db).await {
        Ok(Some(config)) => config,
        _ => {
            tracing::error!("❌ Library config not found when sending request");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response();
        }
    };

    let e2ee_payload = json!({
        "from_peer_url": state.our_public_url(),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(response)) => {
            // Check lender's synchronous response for auto-reject or auto-accept
            if let Some(ref clear_msg) = response {
                let status = clear_msg
                    .payload
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");

                if status == "rejected" {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (E2EE)",
                        outgoing_id
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": "no_available_copy" })),
                    )
                        .into_response();
                }

                if status == "accepted" {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (E2EE)",
                        outgoing_id
                    );
                    // Process acceptance: update outgoing request + create borrowed copy
                    let lender_request_id = clear_msg
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    process_borrower_acceptance(
                        db,
                        &outgoing_id,
                        &clear_msg.payload,
                        lender_request_id.as_deref(),
                    )
                    .await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
            }
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)", "status": "pending" })),
            )
                .into_response();
        }
        Ok(None) => {
            // Peer doesn't support E2EE, fall through to plaintext
        }
        Err(e) => {
            // E2EE transport error - both direct and relay failed.
            // Do NOT fall back to plaintext to avoid duplicate requests.
            tracing::warn!("E2EE send failed (no plaintext fallback): {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to deliver request to peer" })),
            )
                .into_response();
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Cannot reach peer: {}", e) })),
        )
            .into_response();
    }
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            let resp_status = response.status();
            let body = response.text().await.unwrap_or_default();

            if resp_status.is_success() {
                // Parse response body to check for auto-acceptance
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("accepted")
                {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (plaintext)",
                        outgoing_id
                    );
                    let lender_request_id = parsed.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(db, &outgoing_id, &parsed, lender_request_id).await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Request sent", "status": "pending" })),
                )
                    .into_response()
            } else {
                // Parse lender response to check for auto-rejection
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("rejected")
                {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    let reason = parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (plaintext): {}",
                        outgoing_id,
                        reason
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": reason })),
                    )
                        .into_response();
                }
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct BookRequestByUrl {
    peer_url: String,
    book_isbn: String,
    book_title: String,
}

pub async fn request_book_by_url(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<BookRequestByUrl>,
) -> impl IntoResponse {
    let db = state.db();

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(&payload.peer_url);

    // 1. Find peer by URL (may be None for unsaved mDNS peers)
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(db)
        .await
    {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // Unsaved mDNS peer: skip outgoing request tracking, send plaintext directly.
    // SSRF defense (ADR-026): route through ensure_registered_peer_or_mdns so
    // the fallback traversal is logged on the ssrf:mdns target. With
    // allow_unregistered_lan=true, a missing peer row yields Ok(None) and the
    // plaintext request proceeds; registered peers are handled on the main
    // branch above, so only Ok(None) is expected here in practice.
    if peer.is_none() {
        if let Err(e) = validate_url(&docker_url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Invalid peer URL: {}", e) })),
            )
                .into_response();
        }
        if let Err(status) = ensure_registered_peer_or_mdns(db, &docker_url, true).await {
            return status.into_response();
        }

        let my_config = match crate::models::library_config::Entity::find().one(db).await {
            Ok(Some(config)) => config,
            _ => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Library config not found" })),
                )
                    .into_response();
            }
        };

        let request_payload = json!({
            "from_peer_url": state.our_public_url(),
            "from_peer_name": my_config.name,
            "book_isbn": payload.book_isbn,
            "book_title": payload.book_title,
            "requester_request_id": uuid::Uuid::new_v4().to_string()
        });

        let client = get_safe_client();
        let url = format!("{}/api/peers/request", docker_url);
        return match client.post(&url).json(&request_payload).send().await {
            Ok(response) if response.status().is_success() => {
                (StatusCode::OK, Json(json!({ "message": "Request sent" }))).into_response()
            }
            Ok(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Peer rejected request" })),
            )
                .into_response(),
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "Failed to contact peer" })),
            )
                .into_response(),
        };
    }

    let peer = peer.unwrap();

    // 2. Guards: prevent invalid borrow requests.
    {
        use crate::models::{p2p_outgoing_request, p2p_request};

        // 2a. Reject if there is already a pending or accepted outgoing request for this
        //     book from this peer (prevents double-borrowing the same copy).
        let already_borrowing = p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::ToPeerId.eq(peer.id))
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(
                Condition::any()
                    .add(p2p_outgoing_request::Column::Status.eq("pending"))
                    .add(p2p_outgoing_request::Column::Status.eq("accepted")),
            )
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        // 2b. Reject if user is currently lending this book to the same peer
        //     (prevents borrowing back a book that is out on loan to them).
        let currently_lending = p2p_request::Entity::find()
            .filter(p2p_request::Column::FromPeerId.eq(peer.id))
            .filter(p2p_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(p2p_request::Column::Status.eq("accepted"))
            .one(db)
            .await
            .unwrap_or(None)
            .is_some();

        if already_borrowing || currently_lending {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "already_requested" })),
            )
                .into_response();
        }
    }

    // 3. Save Outgoing Request
    let outgoing_id = uuid::Uuid::new_v4().to_string();
    let outgoing = crate::models::p2p_outgoing_request::ActiveModel {
        id: Set(outgoing_id.clone()),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set("pending".to_string()),
        lender_request_id: Set(None),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
    };

    if let Err(e) = crate::models::p2p_outgoing_request::Entity::insert(outgoing)
        .exec(db)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // 4. Send request to peer
    // Note: validate_url is deferred to the plaintext fallback path below.
    // Relay-only peers have a relay:// URL that is valid for E2EE but not for
    // direct HTTP, so SSRF validation must not block the E2EE path.

    // Get my config to identify myself
    let my_config = match crate::models::library_config::Entity::find().one(db).await {
        Ok(Some(config)) => config,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Library config not found" })),
            )
                .into_response();
        }
    };

    let e2ee_payload = json!({
        "from_peer_url": state.our_public_url(),
        "from_peer_name": my_config.name,
        "book_isbn": payload.book_isbn,
        "book_title": payload.book_title,
        "requester_request_id": outgoing_id
    });

    // Try E2EE path first
    match try_send_e2ee(&state, &peer, "loan_request", e2ee_payload.clone()).await {
        Ok(Some(response)) => {
            // Check lender's synchronous response for auto-reject or auto-accept
            if let Some(ref clear_msg) = response {
                let status = clear_msg
                    .payload
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending");
                if status == "rejected" {
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (E2EE)",
                        outgoing_id
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": "no_available_copy" })),
                    )
                        .into_response();
                }

                if status == "accepted" {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (E2EE)",
                        outgoing_id
                    );
                    let lender_request_id = clear_msg
                        .payload
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    process_borrower_acceptance(
                        db,
                        &outgoing_id,
                        &clear_msg.payload,
                        lender_request_id.as_deref(),
                    )
                    .await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
            }
            return (
                StatusCode::OK,
                Json(json!({ "message": "Request sent (encrypted)", "status": "pending" })),
            )
                .into_response();
        }
        Ok(None) => {
            // E2EE not available for this peer — fall back to plaintext.
        }
        Err(e) => {
            // E2EE transport error - both direct and relay failed.
            // Fall through to plaintext: if E2EE could not deliver at all
            // (peer unreachable or decryption failed on their side),
            // there is no duplicate risk.
            tracing::warn!("E2EE loan_request error, falling back to plaintext: {e}");
        }
    }

    // Legacy plaintext path (only reached if E2EE returned Ok(None))
    // SSRF validation: only needed here for direct HTTP to peer URL.
    // Relay-only peers (relay://) never reach this point because E2EE handles them.
    if let Err(e) = validate_url(&peer.url) {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Cannot reach peer: {}", e) })),
        )
            .into_response();
    }
    let client = get_safe_client();
    let url = format!("{}/api/peers/request", peer.url);

    let res = client.post(&url).json(&e2ee_payload).send().await;

    match res {
        Ok(response) => {
            let resp_status = response.status();
            let body = response.text().await.unwrap_or_default();

            if resp_status.is_success() {
                // Parse response body to check for auto-acceptance
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("accepted")
                {
                    tracing::info!(
                        "Outgoing request {} auto-accepted by peer (plaintext)",
                        outgoing_id
                    );
                    let lender_request_id = parsed.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(db, &outgoing_id, &parsed, lender_request_id).await;
                    return (
                        StatusCode::OK,
                        Json(json!({ "message": "Request auto-accepted", "status": "accepted" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({ "message": "Request sent", "status": "pending" })),
                )
                    .into_response()
            } else {
                // Parse lender response to check for auto-rejection
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
                    && parsed.get("status").and_then(|s| s.as_str()) == Some("rejected")
                {
                    // Update outgoing request to rejected
                    let _ = crate::models::p2p_outgoing_request::Entity::update_many()
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::Status,
                            sea_orm::prelude::Expr::value("rejected"),
                        )
                        .col_expr(
                            crate::models::p2p_outgoing_request::Column::UpdatedAt,
                            sea_orm::prelude::Expr::value(chrono::Utc::now().to_rfc3339()),
                        )
                        .filter(crate::models::p2p_outgoing_request::Column::Id.eq(&outgoing_id))
                        .exec(db)
                        .await;
                    let reason = parsed
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown");
                    tracing::info!(
                        "Outgoing request {} auto-rejected by peer (plaintext): {}",
                        outgoing_id,
                        reason
                    );
                    return (
                        StatusCode::OK,
                        Json(json!({ "status": "rejected", "reason": reason })),
                    )
                        .into_response();
                }
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "Peer rejected request" })),
                )
                    .into_response()
            }
        }
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "Failed to contact peer" })),
        )
            .into_response(),
    }
}

pub async fn list_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::book;
    use crate::utils::cover_url::{self, ResolveScope};

    let requests = crate::models::p2p_outgoing_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Look up local books by ISBN so we can link to them (same pattern as list_requests)
    let isbns: Vec<String> = requests
        .iter()
        .map(|(req, _)| req.book_isbn.clone())
        .filter(|isbn| !isbn.is_empty())
        .collect();

    // Cover URLs must be servable by `CachedNetworkImage` on the UI. A raw
    // filesystem path in `books.cover_url` (typical of a local upload before
    // hub sync) would render a placeholder, so the rewrite to a hub URL or
    // `/api/books/{id}/cover` fallback happens here, keyed on the same scope
    // as `api/books.rs` list endpoint.
    let hub_prefix = crate::models::Book::hub_cover_prefix(&db).await;
    let mut isbn_book_map: std::collections::HashMap<String, (i32, Option<String>)> =
        std::collections::HashMap::new();
    if !isbns.is_empty()
        && let Ok(books) = book::Entity::find()
            .filter(book::Column::Isbn.is_in(isbns))
            .all(&db)
            .await
    {
        for b in books {
            if let Some(isbn) = &b.isbn {
                let resolved = cover_url::resolve_single(
                    b.cover_url.as_deref(),
                    b.id,
                    Some(&b.updated_at),
                    hub_prefix.as_deref(),
                    ResolveScope::Lan,
                )
                .unwrap_or(None);
                isbn_book_map.insert(isbn.clone(), (b.id, resolved));
            }
        }
    }

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            let book_info = isbn_book_map.get(&req.book_isbn);
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "book_id": book_info.map(|(id, _)| *id),
                "cover_url": book_info.and_then(|(_, url)| url.clone()),
                "status": req.status,
                "created_at": req.created_at,
                "updated_at": req.updated_at,
                "peer_id": peer.as_ref().map(|p| p.id),
                "peer_name": peer.as_ref().map(|p| p.name.clone()).unwrap_or("Unknown".to_string()),
                "peer_url": peer.map(|p| p.url)
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

/// Delete all non-pending outgoing requests (cleanup).
pub async fn clear_outgoing_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use sea_orm::ConnectionTrait;
    let result = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM p2p_outgoing_requests WHERE status != 'pending'".to_owned(),
        ))
        .await;

    match result {
        Ok(r) => (
            StatusCode::OK,
            Json(json!({ "deleted": r.rows_affected() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Sync pending outgoing requests by querying each lender for current status.
///
/// For each pending outgoing request, sends a `request_status_query` via E2EE
/// (with relay fallback) to the lender. If the lender reports the request has been
/// accepted/rejected, updates the local outgoing request accordingly and creates
/// the borrowed copy if accepted.
pub async fn sync_outgoing_requests(
    State(state): State<crate::infrastructure::AppState>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    let db = state.db();
    let pending = p2p_outgoing_request::Entity::find()
        .filter(p2p_outgoing_request::Column::Status.eq("pending"))
        .all(db)
        .await
        .unwrap_or_default();

    if pending.is_empty() {
        return (StatusCode::OK, Json(json!({ "synced": 0, "updated": 0 }))).into_response();
    }

    let mut synced = 0u32;
    let mut updated = 0u32;

    for outgoing in &pending {
        // Find the lender peer
        let lender = match peer::Entity::find_by_id(outgoing.to_peer_id).one(db).await {
            Ok(Some(p)) => p,
            _ => continue,
        };

        let query_payload = json!({
            "requester_request_id": outgoing.id,
        });

        // Try E2EE (with relay fallback)
        let result = try_send_e2ee(&state, &lender, "request_status_query", query_payload).await;
        synced += 1;

        if let Ok(Some(Some(ref clear_msg))) = result {
            let remote_status = clear_msg
                .payload
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");

            if remote_status != "pending" && remote_status != "not_found" {
                tracing::info!(
                    "Sync: outgoing request {} status changed to '{}'",
                    outgoing.id,
                    remote_status
                );

                if remote_status == "accepted" {
                    let lender_request_id =
                        clear_msg.payload.get("request_id").and_then(|v| v.as_str());
                    process_borrower_acceptance(
                        db,
                        &outgoing.id,
                        &clear_msg.payload,
                        lender_request_id,
                    )
                    .await;
                } else {
                    // rejected or returned
                    let mut active: p2p_outgoing_request::ActiveModel = outgoing.clone().into();
                    active.status = Set(remote_status.to_string());
                    active.updated_at = Set(Utc::now().to_rfc3339());
                    let _ = active.update(db).await;
                }
                updated += 1;
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "synced": synced, "updated": updated })),
    )
        .into_response()
}

/// Delete all non-pending incoming requests (cleanup).
pub async fn clear_incoming_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use sea_orm::ConnectionTrait;
    let result = db
        .execute(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM p2p_requests WHERE status != 'pending'".to_owned(),
        ))
        .await;

    match result {
        Ok(r) => (
            StatusCode::OK,
            Json(json!({ "deleted": r.rows_affected() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct IncomingRequest {
    from_peer_url: String,
    from_peer_name: String,
    book_isbn: String,
    book_title: String,
    requester_request_id: Option<String>,
}

pub async fn receive_request(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<IncomingRequest>,
) -> impl IntoResponse {
    let db = state.db().clone();

    // 1. Find or Create Peer
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.from_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_peer_name),
                url: Set(payload.from_peer_url),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
                updated_at: Set(chrono::Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
            }
        }
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "DB Error" })),
            )
                .into_response();
        }
    };

    // 2. Check copy availability and guard against duplicate active loans.
    let has_available_copy = {
        use crate::models::book;
        use crate::models::copy;

        let book_found = book::Entity::find()
            .filter(book::Column::Isbn.eq(&payload.book_isbn))
            .one(&db)
            .await
            .unwrap_or(None);

        if let Some(b) = book_found {
            copy::Entity::find()
                .filter(copy::Column::BookId.eq(b.id))
                .filter(copy::Column::Status.eq("available"))
                .one(&db)
                .await
                .unwrap_or(None)
                .is_some()
        } else {
            false
        }
    };

    // Guard: reject if this peer already has a pending or accepted request for this book
    //        (defense-in-depth — borrower side should catch this first).
    let already_has_active_request = {
        use crate::models::p2p_request;
        p2p_request::Entity::find()
            .filter(p2p_request::Column::FromPeerId.eq(peer.id))
            .filter(p2p_request::Column::BookIsbn.eq(&payload.book_isbn))
            .filter(
                Condition::any()
                    .add(p2p_request::Column::Status.eq("pending"))
                    .add(p2p_request::Column::Status.eq("accepted")),
            )
            .one(&db)
            .await
            .unwrap_or(None)
            .is_some()
    };

    // 3. Check if auto-approve should be used
    let auto_approve =
        is_auto_approve_loans_enabled(&db).await && peer.connection_status == "accepted";

    // Determine initial status: auto-reject if no copy available or duplicate request
    let initial_status = if !has_available_copy || already_has_active_request {
        "rejected"
    } else {
        "pending"
    };

    // 4. Create Request Record
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = crate::models::p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn.clone()),
        book_title: Set(payload.book_title.clone()),
        status: Set(initial_status.to_string()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        requester_request_id: Set(payload.requester_request_id.clone()),
    };

    match crate::models::p2p_request::Entity::insert(request)
        .exec(&db)
        .await
    {
        Ok(_) => {
            // Auto-rejected: no available copy or duplicate active request
            if !has_available_copy || already_has_active_request {
                let reason = if already_has_active_request {
                    "already_borrowed"
                } else {
                    "no_available_copy"
                };
                tracing::info!(
                    "Auto-rejected loan request {} for '{}' - {}",
                    request_id,
                    payload.book_title,
                    reason
                );
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "success": false, "status": "rejected", "reason": reason })),
                )
                    .into_response();
            }

            // If auto-approve is enabled, immediately accept the request
            if auto_approve {
                tracing::info!(
                    "Auto-approving loan request {} for peer {}",
                    request_id,
                    peer.name
                );
                match perform_loan_acceptance(
                    &db,
                    &request_id,
                    &payload.book_isbn,
                    &payload.book_title,
                    &peer,
                )
                .await
                {
                    Ok(result) => {
                        // Emit borrow_request notification (auto-approved)
                        crate::services::notification_service::emit(
                            &db,
                            crate::domain::CreateNotification {
                                event_type: crate::domain::NotificationEventType::BorrowRequest,
                                title: payload.book_title.clone(),
                                body: Some(peer.name.clone()),
                                ref_type: Some("peer".to_string()),
                                ref_id: Some(request_id.clone()),
                            },
                        )
                        .await;

                        // Fire-and-forget: try to notify borrower via E2EE (with relay fallback)
                        let confirm_payload = json!({
                            "isbn": result.book_isbn,
                            "title": result.book_title,
                            "cover_url": result.book_cover_url,
                            "lender_name": result.lender_name,
                            "due_date": result.due_date,
                            "request_id": request_id,
                            "requester_request_id": payload.requester_request_id,
                        });
                        let _ = try_send_e2ee(&state, &peer, "loan_confirmation", confirm_payload)
                            .await;

                        return (
                            StatusCode::OK,
                            Json(json!({
                                "success": true,
                                "status": "accepted",
                                "request_id": request_id,
                                "due_date": result.due_date,
                                "lender_name": result.lender_name,
                                "isbn": result.book_isbn,
                                "title": result.book_title,
                                "cover_url": result.book_cover_url,
                            })),
                        )
                            .into_response();
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Auto-approve failed for request {}: {} - staying pending",
                            request_id,
                            e
                        );
                        // Fall through to return "pending"
                    }
                }
            }

            // Emit borrow_request notification (only when NOT auto-approved)
            crate::services::notification_service::emit(
                &db,
                crate::domain::CreateNotification {
                    event_type: crate::domain::NotificationEventType::BorrowRequest,
                    title: payload.book_title.clone(),
                    body: Some(peer.name.clone()),
                    ref_type: Some("peer".to_string()),
                    ref_id: Some(request_id.clone()),
                },
            )
            .await;

            (
                StatusCode::CREATED,
                Json(json!({ "success": true, "status": "pending" })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_requests(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::book;
    use crate::utils::cover_url::{self, ResolveScope};

    let requests = crate::models::p2p_request::Entity::find()
        .find_also_related(peer::Entity)
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Collect unique ISBNs to look up local books
    let isbns: Vec<String> = requests
        .iter()
        .map(|(req, _)| req.book_isbn.clone())
        .filter(|isbn| !isbn.is_empty())
        .collect();

    // Cover URLs must be servable by `CachedNetworkImage` on the UI. A raw
    // filesystem path in `books.cover_url` (typical of a local upload before
    // hub sync) would render a placeholder on the owner's list of incoming
    // requests, so the rewrite to a hub URL or `/api/books/{id}/cover`
    // fallback happens here, keyed on the same LAN scope as `api/books.rs`.
    let hub_prefix = crate::models::Book::hub_cover_prefix(&db).await;
    let mut isbn_book_map: std::collections::HashMap<String, (i32, Option<String>)> =
        std::collections::HashMap::new();
    if !isbns.is_empty()
        && let Ok(books) = book::Entity::find()
            .filter(book::Column::Isbn.is_in(isbns))
            .all(&db)
            .await
    {
        for b in books {
            if let Some(isbn) = &b.isbn {
                let resolved = cover_url::resolve_single(
                    b.cover_url.as_deref(),
                    b.id,
                    Some(&b.updated_at),
                    hub_prefix.as_deref(),
                    ResolveScope::Lan,
                )
                .unwrap_or(None);
                isbn_book_map.insert(isbn.clone(), (b.id, resolved));
            }
        }
    }

    let dtos: Vec<serde_json::Value> = requests
        .into_iter()
        .map(|(req, peer)| {
            let book_info = isbn_book_map.get(&req.book_isbn);
            json!({
                "id": req.id,
                "book_title": req.book_title,
                "book_isbn": req.book_isbn,
                "book_id": book_info.map(|(id, _)| *id),
                "cover_url": book_info.and_then(|(_, url)| url.clone()),
                "status": req.status,
                "created_at": req.created_at,
                "updated_at": req.updated_at,
                "peer_id": peer.as_ref().map(|p| p.id),
                "peer_name": peer.as_ref().map(|p| p.name.clone()).unwrap_or("Unknown".to_string()),
                "peer_url": peer.map(|p| p.url)
            })
        })
        .collect();

    (StatusCode::OK, Json(dtos)).into_response()
}

#[derive(Deserialize)]
pub struct RequestAction {
    pub status: String,
}

pub async fn update_request_status(
    State(state): State<crate::infrastructure::AppState>,
    Path(id): Path<String>,
    Json(payload): Json<RequestAction>,
) -> impl IntoResponse {
    use crate::models::{book, contact, copy, loan, p2p_request};
    let db = state.db().clone();

    let req = match p2p_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(r)) => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response();
        }
    };

    let mut active: p2p_request::ActiveModel = req.clone().into();
    let new_status = payload.status.as_str();

    // State transition logic
    if new_status == "accepted" && req.status == "pending" {
        // 1. Find Peer to link/create Contact
        let peer = match peer::Entity::find_by_id(req.from_peer_id).one(&db).await {
            Ok(Some(p)) => p,
            _ => {
                // Peer no longer exists: auto-reject the request since we
                // cannot create the contact/loan without peer info.
                tracing::warn!(
                    "Peer {} not found for request {} - auto-rejecting",
                    req.from_peer_id,
                    req.id
                );
                active.status = Set("rejected".to_string());
                active.updated_at = Set(chrono::Utc::now().to_rfc3339());
                let _ = active.update(&db).await;
                return (
                    StatusCode::OK,
                    Json(json!({ "message": "Request auto-rejected: peer no longer available" })),
                )
                    .into_response();
            }
        };

        // 2. Find Book and Available Copy
        tracing::info!(
            "Looking for book with ISBN: '{}' for request {}",
            req.book_isbn,
            req.id
        );
        let book = match book::Entity::find()
            .filter(book::Column::Isbn.eq(&req.book_isbn))
            .one(&db)
            .await
        {
            Ok(Some(b)) => {
                tracing::info!("Found book: {} (id={})", b.title, b.id);
                b
            }
            Ok(None) => {
                tracing::warn!(
                    "Book not found for ISBN: '{}'. Checking by title: '{}'",
                    req.book_isbn,
                    req.book_title
                );
                // Fallback: Try to find by title if ISBN lookup fails
                match book::Entity::find()
                    .filter(book::Column::Title.eq(&req.book_title))
                    .one(&db)
                    .await
                {
                    Ok(Some(b)) => {
                        tracing::info!("Found book by title: {} (id={})", b.title, b.id);
                        b
                    }
                    _ => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({ "error": format!("Book not found (ISBN: '{}', Title: '{}')", req.book_isbn, req.book_title) })),
                        )
                            .into_response()
                    }
                }
            }
            Err(e) => {
                tracing::error!("DB error looking up book: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("DB error: {}", e) })),
                )
                    .into_response();
            }
        };

        let copy = match copy::Entity::find()
            .filter(copy::Column::BookId.eq(book.id))
            .filter(copy::Column::Status.eq("available"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            _ => {
                // Self-healing: Check if ANY copy exists
                let any_copy = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if any_copy.is_none() {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No copy found" })),
                    )
                        .into_response();
                } else {
                    // Copies exist but none are available (truly borrowed)
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "No available copies" })),
                    )
                        .into_response();
                }
            }
        };

        // 3. Find or Create Contact for Peer
        let contact = match contact::Entity::find()
            .filter(contact::Column::Name.eq(&peer.name))
            .filter(contact::Column::Type.eq("Library"))
            .one(&db)
            .await
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                // Create new contact
                let new_contact =
                    contact::ActiveModel {
                        r#type: Set("Library".to_string()),
                        name: Set(peer.name.clone()),
                        library_owner_id: Set(
                            match crate::utils::library_helpers::resolve_library_id(&db).await {
                                Ok(id) => id,
                                Err(e) => {
                                    return (
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        Json(json!({ "error": format!("No library: {}", e) })),
                                    )
                                        .into_response();
                                }
                            },
                        ),
                        is_active: Set(true),
                        created_at: Set(chrono::Utc::now().to_rfc3339()),
                        updated_at: Set(chrono::Utc::now().to_rfc3339()),
                        ..Default::default()
                    };
                match new_contact.insert(&db).await {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("Failed to create contact: {}", e) })),
                        )
                            .into_response();
                    }
                }
            }
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "DB Error finding contact" })),
                )
                    .into_response();
            }
        };

        // 4. Create Loan
        let loan = loan::ActiveModel {
            copy_id: Set(copy.id),
            contact_id: Set(contact.id),
            library_id: Set(
                match crate::utils::library_helpers::resolve_library_id(&db).await {
                    Ok(id) => id,
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("No library: {}", e) })),
                        )
                            .into_response();
                    }
                },
            ),
            loan_date: Set(chrono::Utc::now().to_rfc3339()),
            due_date: Set((chrono::Utc::now()
                + chrono::Duration::days(resolve_loan_duration_days(&db, book.id).await))
            .to_rfc3339()),
            status: Set("active".to_string()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
            updated_at: Set(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        if let Err(e) = loan::Entity::insert(loan).exec(&db).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to create loan: {}", e) })),
            )
                .into_response();
        }

        // Update Copy status
        let mut active_copy: copy::ActiveModel = copy.into();
        active_copy.status = Set("loaned".to_string());
        info!(
            "Updating copy {} status to 'loaned' for loan acceptance",
            active_copy.id.clone().unwrap()
        );
        if let Err(e) = active_copy.update(&db).await {
            error!("Failed to update copy status to 'lent': {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to update copy status: {}", e) })),
            )
                .into_response();
        }

        // 5. Notify borrower that loan was accepted
        let peer_url = peer.url.clone();
        let book_isbn = book.isbn.clone();
        let book_title = book.title.clone();
        let hub_prefix = crate::models::Book::hub_cover_prefix(&db).await;
        // Borrower notification may travel via hub relay; use relay-safe
        // variant so unreachable local paths are stripped rather than sent.
        let book_cover = crate::models::Book::safe_cover_url_for_relay(
            book.cover_url.as_deref(),
            book.id,
            Some(book.updated_at.as_str()),
            hub_prefix.as_deref(),
        );
        let due_date = (chrono::Utc::now()
            + chrono::Duration::days(resolve_loan_duration_days(&db, book.id).await))
        .format("%Y-%m-%d")
        .to_string();

        // Get library name for lender identification
        let lender_name = match crate::models::library::Entity::find_by_id(1).one(&db).await {
            Ok(Some(lib)) => lib.name,
            _ => "Unknown Library".to_string(),
        };

        let confirm_payload = serde_json::json!({
            "isbn": book_isbn,
            "title": book_title,
            "author": Option::<String>::None,
            "cover_url": book_cover,
            "lender_name": lender_name,
            "due_date": due_date,
            "request_id": req.id,
            "requester_request_id": req.requester_request_id,
        });

        // Try E2EE path first
        match try_send_e2ee(&state, &peer, "loan_confirmation", confirm_payload.clone()).await {
            Ok(Some(_)) => {
                tracing::info!("E2EE: Loan confirmation sent to {} (encrypted)", peer.name);
            }
            Err(e) => {
                // E2EE transport error — message MAY have been delivered.
                // Do NOT fall back to plaintext to avoid duplicate borrowed copies.
                tracing::warn!("E2EE: Loan confirmation error (no plaintext fallback): {e}");
            }
            Ok(None) => {
                // E2EE not available for this peer — fall back to plaintext
                let peer_url_clone = peer_url.clone();
                tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    let confirm_result = client
                        .post(format!("{}/api/peers/loans/confirm", peer_url_clone))
                        .json(&confirm_payload)
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await;

                    match confirm_result {
                        Ok(resp) => {
                            tracing::info!(
                                "Loan confirmation sent to {}: {}",
                                peer_url_clone,
                                resp.status()
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Failed to send loan confirmation to {}: {}",
                                peer_url_clone,
                                e
                            );
                        }
                    }
                });
            }
        }
    } else if new_status == "returned" && req.status == "accepted" {
        // Handle Return
        // Find the loan associated with this peer (contact) and book
        // This is tricky because we didn't link Loan to Request directly.
        // We have to infer: Find active loan for this book's copy where contact matches peer.

        // 1. Find Peer/Contact (graceful: if peer is gone, still update request status)
        let peer_opt = peer::Entity::find_by_id(req.from_peer_id)
            .one(&db)
            .await
            .ok()
            .flatten();

        if peer_opt.is_none() {
            tracing::warn!(
                "Peer {} not found for return of request {} - updating request status only",
                req.from_peer_id,
                req.id
            );
        }

        let contact = match &peer_opt {
            Some(peer) => contact::Entity::find()
                .filter(contact::Column::Name.eq(&peer.name))
                .filter(contact::Column::Type.eq("Library"))
                .one(&db)
                .await
                .unwrap_or(None),
            None => None,
        };

        if let Some(contact) = contact {
            let book = book::Entity::find()
                .filter(book::Column::Isbn.eq(&req.book_isbn))
                .one(&db)
                .await
                .unwrap_or(None);

            if let Some(book) = book {
                // 3. Find Active Loan for any copy of this book for this contact
                let copies = copy::Entity::find()
                    .filter(copy::Column::BookId.eq(book.id))
                    .all(&db)
                    .await
                    .unwrap_or(vec![]);

                let copy_ids: Vec<i32> = copies.iter().map(|c| c.id).collect();

                let active_loan = loan::Entity::find()
                    .filter(loan::Column::ContactId.eq(contact.id))
                    .filter(loan::Column::Status.eq("active"))
                    .filter(loan::Column::CopyId.is_in(copy_ids))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if let Some(l) = active_loan {
                    let mut active_loan: loan::ActiveModel = l.clone().into();
                    active_loan.status = Set("returned".to_string());
                    active_loan.return_date = Set(Some(chrono::Utc::now().to_rfc3339()));
                    active_loan.updated_at = Set(chrono::Utc::now().to_rfc3339());
                    let _ = active_loan.update(&db).await;

                    // Update Copy
                    if let Some(copy) = copy::Entity::find_by_id(l.copy_id)
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                    {
                        let mut active_copy: copy::ActiveModel = copy.into();
                        active_copy.status = Set("available".to_string());
                        let _ = active_copy.update(&db).await;
                    }

                    // Emit book_returned notification
                    let peer_name = peer_opt
                        .as_ref()
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                    crate::services::notification_service::emit(
                        &db,
                        crate::domain::CreateNotification {
                            event_type: crate::domain::NotificationEventType::BookReturned,
                            title: book.title.clone(),
                            body: Some(peer_name),
                            ref_type: Some("loan".to_string()),
                            ref_id: Some(req.id.clone()),
                        },
                    )
                    .await;
                }
            }
        }
    }

    // Update Request Status
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    // Notify borrower of status change
    let peer_for_notify = peer::Entity::find_by_id(req.from_peer_id)
        .one(&db)
        .await
        .ok()
        .flatten();

    if let Some(peer) = peer_for_notify {
        // Use the borrower's original request ID so they can match
        // the status update to their outgoing request. Fall back to
        // our local ID for backward compat with old peers.
        let borrower_loan_id = req
            .requester_request_id
            .clone()
            .unwrap_or_else(|| req.id.clone());

        let status_payload = json!({
            "loan_id": borrower_loan_id,
            "status": new_status,
        });

        // Try E2EE first
        match try_send_e2ee(&state, &peer, "status_update", status_payload).await {
            Ok(Some(_)) => {
                tracing::info!("E2EE: Status update sent to {} (encrypted)", peer.name);
            }
            Err(e) => {
                // E2EE transport error — message MAY have been delivered.
                // Do NOT fall back to plaintext to avoid duplicate status updates.
                tracing::warn!("E2EE: Status update error (no plaintext fallback): {e}");
            }
            Ok(None) => {
                // E2EE not available for this peer — fall back to plaintext
                let peer_url = peer.url.clone();
                let request_id = borrower_loan_id;
                let status_to_send = new_status.to_string();

                tokio::spawn(async move {
                    let client = get_safe_client();
                    let notify_url =
                        format!("{}/api/peers/requests/status/{}", peer_url, request_id);

                    tracing::info!(
                        "Notifying borrower {} of status change: {} -> {}",
                        peer_url,
                        request_id,
                        status_to_send
                    );

                    match client
                        .put(&notify_url)
                        .json(&serde_json::json!({ "status": status_to_send }))
                        .send()
                        .await
                    {
                        Ok(res) => {
                            tracing::info!("Borrower notified: {}", res.status());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to notify borrower: {}", e);
                        }
                    }
                });
            }
        }
    }

    match active.update(&db).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn list_peer_books(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Check if peer is approved
    if let Ok(Some(peer)) = peer::Entity::find_by_id(peer_id).one(&db).await
        && !is_peer_approved(&db, &peer).await
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer_id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// List peer books by URL (solves ID mismatch)
pub async fn list_peer_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found with URL: {}", docker_url) })),
            )
                .into_response();
        }
    };

    // Check if peer is approved
    if !is_peer_approved(&db, &peer).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Peer connection pending approval" })),
        )
            .into_response();
    }

    // Get books for this peer
    let books = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    (StatusCode::OK, Json(books)).into_response()
}

/// Get cached peer books with staleness metadata (no network call to peer)
/// Returns books from local cache along with last_synced timestamp for UI staleness indicator
pub async fn get_cached_books_by_url(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::peer_book;

    // Extract URL from payload
    let peer_url = match payload.get("url").and_then(|v| v.as_str()) {
        Some(url) => url,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'url' field" })),
            )
                .into_response();
        }
    };

    // Translate localhost URL to Docker service name if needed
    let docker_url = translate_url_for_docker(peer_url);

    // Find peer by URL
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&docker_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Peer not found - return empty result with null metadata
            return (
                StatusCode::OK,
                Json(json!({
                    "books": [],
                    "peer_name": null,
                    "peer_id": null,
                    "last_synced": null,
                    "last_seen": null,
                    "cached": true
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    // Get cached books for this peer
    let cached = peer_book::Entity::find()
        .filter(peer_book::Column::PeerId.eq(peer.id))
        .all(&db)
        .await
        .unwrap_or(vec![]);

    // Get latest synced_at from cached books (all books have same sync time)
    let last_synced = cached.first().map(|b| b.synced_at.clone());

    // Convert peer_book rows to Book DTOs so id == remote_book_id (matches the
    // live P2P shape) and first_seen_at flows through for the "new" badge.
    let books: Vec<crate::models::Book> = cached.into_iter().map(Into::into).collect();

    (
        StatusCode::OK,
        Json(json!({
            "books": books,
            "peer_name": peer.name,
            "peer_id": peer.id,
            "last_synced": last_synced,
            "last_seen": peer.last_seen,
            "cached": true
        })),
    )
        .into_response()
}

/// Cleanup peer_books entries older than 30 days (TTL for privacy)
/// Call this on app startup to auto-purge stale caches
pub async fn cleanup_stale_peer_books(State(db): State<DatabaseConnection>) -> impl IntoResponse {
    use crate::models::peer_book;
    use sea_orm::QueryFilter;

    // Calculate cutoff date (30 days ago)
    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let cutoff_str = cutoff.to_rfc3339();

    // Delete stale peer_books entries
    let books_deleted = peer_book::Entity::delete_many()
        .filter(peer_book::Column::SyncedAt.lt(&cutoff_str))
        .exec(&db)
        .await
        .map(|r| r.rows_affected)
        .unwrap_or(0);

    // Also clean up stale peer_gamification_stats
    let stats_deleted = peer_gamification_stats::Entity::delete_many()
        .filter(peer_gamification_stats::Column::SyncedAt.lt(&cutoff_str))
        .exec(&db)
        .await
        .map(|r| r.rows_affected)
        .unwrap_or(0);

    if books_deleted > 0 || stats_deleted > 0 {
        tracing::info!(
            "TTL cleanup: deleted {} stale peer_books + {} stale peer_gamification_stats (older than 30 days)",
            books_deleted,
            stats_deleted
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "deleted": books_deleted,
            "stats_deleted": stats_deleted,
            "cutoff": cutoff_str
        })),
    )
        .into_response()
}

pub async fn delete_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    match p2p_request::Entity::delete_by_id(id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "Request not found" })),
                )
                    .into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_outgoing_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    // 1. First, retrieve the request to get the peer info
    let request = match p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Request not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 2. Get the peer URL to notify them
    let peer = match peer::Entity::find_by_id(request.to_peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::warn!(
                "Peer {} not found for outgoing request {}",
                request.to_peer_id,
                id
            );
            // Peer not found, just delete locally
            let _ = p2p_outgoing_request::Entity::delete_by_id(&id)
                .exec(&db)
                .await;
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 3. Notify the peer about the cancellation (best effort)
    let client = get_safe_client();
    let cancel_url = format!("{}/api/peers/requests/cancel/{}", peer.url, id);

    tracing::info!(
        "📡 Notifying peer {} of request cancellation: {}",
        peer.name,
        cancel_url
    );

    match client.delete(&cancel_url).send().await {
        Ok(res) => {
            if res.status().is_success() {
                tracing::info!("✅ Peer notified successfully");
            } else {
                tracing::warn!("⚠️ Peer notification returned: {}", res.status());
            }
        }
        Err(e) => {
            tracing::warn!("⚠️ Failed to notify peer (may be offline): {}", e);
            // Continue with local deletion anyway
        }
    }

    // 4. Delete locally
    match p2p_outgoing_request::Entity::delete_by_id(&id)
        .exec(&db)
        .await
    {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Receive cancellation notification from a peer who cancelled their outgoing request
pub async fn cancel_request(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::models::p2p_request;

    tracing::info!("📨 Received cancellation notification for request: {}", id);

    // Delete the incoming request that matches this ID
    match p2p_request::Entity::delete_by_id(&id).exec(&db).await {
        Ok(res) => {
            if res.rows_affected == 0 {
                tracing::warn!("Cancellation target not found: {}", id);
                // Return OK anyway - idempotent behavior
                StatusCode::OK.into_response()
            } else {
                tracing::info!("✅ Request {} cancelled successfully", id);
                StatusCode::OK.into_response()
            }
        }
        Err(e) => {
            tracing::error!("❌ Failed to cancel request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Receive status update notification from lender (updates local outgoing request)
pub async fn update_outgoing_status(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    use crate::models::{book, copy, p2p_outgoing_request};

    let new_status = match payload.get("status").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing status field" })),
            )
                .into_response();
        }
    };

    tracing::info!(
        "📨 Received status update for outgoing request {}: {}",
        id,
        new_status
    );

    // Find the outgoing request
    let request = match p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await {
        Ok(Some(req)) => req,
        Ok(None) => {
            tracing::warn!("Outgoing request not found: {}", id);
            // Return OK anyway - idempotent
            return StatusCode::OK.into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // Clone book_isbn before converting to ActiveModel (need it for cleanup)
    let book_isbn = request.book_isbn.clone();

    // Update the status
    let mut active: p2p_outgoing_request::ActiveModel = request.into();
    active.status = Set(new_status.to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(_) => {
            tracing::info!("✅ Outgoing request {} updated to {}", id, new_status);

            // If the loan is returned, clean up the borrowed copy
            if new_status == "returned" {
                tracing::info!("🧹 Cleaning up borrowed copy for book ISBN: {}", book_isbn);

                // 1. Find the book by ISBN
                if let Ok(Some(book)) = book::Entity::find()
                    .filter(book::Column::Isbn.eq(&book_isbn))
                    .one(&db)
                    .await
                {
                    // 2. Find and delete the borrowed copy
                    if let Ok(Some(borrowed_copy)) = copy::Entity::find()
                        .filter(copy::Column::BookId.eq(book.id))
                        .filter(copy::Column::Status.eq("borrowed"))
                        .one(&db)
                        .await
                    {
                        match copy::Entity::delete_by_id(borrowed_copy.id).exec(&db).await {
                            Err(e) => {
                                tracing::warn!("⚠️ Failed to delete borrowed copy: {}", e);
                            }
                            _ => {
                                tracing::info!(
                                    "✅ Deleted borrowed copy {} for book {}",
                                    borrowed_copy.id,
                                    book.id
                                );
                            }
                        }
                    }

                    // 3. Check if book should be deleted
                    // Conditions: owned=false, reading_status != wishlist, no copies left
                    let should_delete_book = !book.owned
                        && book.reading_status != "wanting"
                        && copy::Entity::find()
                            .filter(copy::Column::BookId.eq(book.id))
                            .count(&db)
                            .await
                            .unwrap_or(1)
                            == 0;

                    if should_delete_book {
                        tracing::info!(
                            "🗑️ Book {} (ISBN: {}) has no more copies, not owned, not in wishlist - deleting",
                            book.id,
                            book_isbn
                        );
                        match book::Entity::delete_by_id(book.id).exec(&db).await {
                            Err(e) => {
                                tracing::warn!("⚠️ Failed to delete book: {}", e);
                            }
                            _ => {
                                tracing::info!("✅ Deleted book {} after loan return", book.id);
                            }
                        }
                    }
                }
            }

            // Emit book_returned notification on borrower side
            if new_status == "returned" {
                let book_title = book::Entity::find()
                    .filter(book::Column::Isbn.eq(&book_isbn))
                    .one(&db)
                    .await
                    .ok()
                    .flatten()
                    .map(|b| b.title)
                    .unwrap_or_else(|| book_isbn.clone());
                let lender_name = if let Ok(Some(req)) =
                    p2p_outgoing_request::Entity::find_by_id(&id).one(&db).await
                {
                    peer::Entity::find_by_id(req.to_peer_id)
                        .one(&db)
                        .await
                        .ok()
                        .flatten()
                        .map(|p| p.name)
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                crate::services::notification_service::emit(
                    &db,
                    crate::domain::CreateNotification {
                        event_type: crate::domain::NotificationEventType::BookReturned,
                        title: book_title,
                        body: Some(lender_name),
                        ref_type: Some("loan".to_string()),
                        ref_id: Some(id.clone()),
                    },
                )
                .await;
            }

            StatusCode::OK.into_response()
        }
        Err(e) => {
            tracing::error!("❌ Failed to update outgoing request: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// ============ BORROWER-INITIATED RETURN ============

#[derive(Deserialize)]
pub struct ReturnBorrowedBookPayload {
    pub copy_id: i32,
}

/// Borrower initiates a return: notifies the lender and cleans up local data.
pub async fn return_borrowed_book(
    State(state): State<crate::infrastructure::AppState>,
    Json(payload): Json<ReturnBorrowedBookPayload>,
) -> impl IntoResponse {
    use crate::models::{book, copy, p2p_outgoing_request, peer};
    let db = state.db().clone();

    tracing::info!(
        "📚 Borrower initiating return for copy_id: {}",
        payload.copy_id
    );

    // 1. Look up the copy to get book_id, then the book to get ISBN
    let the_copy = match copy::Entity::find_by_id(payload.copy_id).one(&db).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Copy not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let book_isbn = match book::Entity::find_by_id(the_copy.book_id).one(&db).await {
        Ok(Some(b)) => b.isbn.unwrap_or_default(),
        _ => String::new(),
    };

    // 2. Find the outgoing request for this book with status "accepted"
    let outgoing = if !book_isbn.is_empty() {
        p2p_outgoing_request::Entity::find()
            .filter(p2p_outgoing_request::Column::BookIsbn.eq(&book_isbn))
            .filter(p2p_outgoing_request::Column::Status.eq("accepted"))
            .one(&db)
            .await
    } else {
        Ok(None)
    };

    let outgoing_req = match outgoing {
        Ok(Some(req)) => req,
        Ok(None) => {
            tracing::warn!(
                "No accepted outgoing request found for ISBN: '{}'. Falling back to local cleanup.",
                book_isbn
            );
            // Fallback: delete the local copy + clean up orphaned book
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            cleanup_orphaned_book(&db, the_copy.book_id).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Copy deleted (no outgoing request found)" })),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("DB error finding outgoing request: {}", e);
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            cleanup_orphaned_book(&db, the_copy.book_id).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Copy deleted (db error on request lookup)" })),
            )
                .into_response();
        }
    };

    // 2. Find the peer (lender)
    let peer = match peer::Entity::find_by_id(outgoing_req.to_peer_id)
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            tracing::warn!("Peer not found for outgoing request");
            // Still clean up locally
            let _ = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await;
            cleanup_orphaned_book(&db, the_copy.book_id).await;
            let mut active: p2p_outgoing_request::ActiveModel = outgoing_req.into();
            active.status = Set("returned".to_string());
            active.updated_at = Set(chrono::Utc::now().to_rfc3339());
            let _ = active.update(&db).await;
            return (
                StatusCode::OK,
                Json(json!({ "message": "Returned (peer not found)" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    // 3. Notify the lender to mark the loan as returned
    let lender_request_id = outgoing_req.lender_request_id.clone();
    if let Some(ref lender_req_id) = lender_request_id {
        let return_payload = json!({
            "loan_id": lender_req_id,
            "status": "returned",
        });

        // Try E2EE first
        match try_send_e2ee(&state, &peer, "status_update", return_payload).await {
            Ok(Some(_)) => {
                tracing::info!(
                    "E2EE: Return notification sent to {} (encrypted)",
                    peer.name
                );
            }
            Err(e) => {
                tracing::warn!("E2EE: Return notification error: {e}");
            }
            Ok(None) => {
                // Plaintext fallback
                let peer_url = peer.url.clone();
                let req_id = lender_req_id.clone();
                tokio::spawn(async move {
                    let client = get_safe_client();
                    let url = format!("{}/api/peers/requests/{}", peer_url, req_id);
                    match client
                        .put(&url)
                        .json(&serde_json::json!({ "status": "returned" }))
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await
                    {
                        Ok(res) => {
                            tracing::info!("Return notification sent to lender: {}", res.status());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to send return notification to lender: {}", e);
                        }
                    }
                });
            }
        }
    } else {
        tracing::warn!(
            "No lender_request_id on outgoing request — cannot notify lender. \
             Lender will need to mark the return manually."
        );
    }

    // 4. Update outgoing request status to "returned"
    let mut active: p2p_outgoing_request::ActiveModel = outgoing_req.into();
    active.status = Set("returned".to_string());
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());
    if let Err(e) = active.update(&db).await {
        tracing::warn!("Failed to update outgoing request status: {}", e);
    }

    // 5. Delete the borrowed copy
    if let Err(e) = copy::Entity::delete_by_id(payload.copy_id).exec(&db).await {
        tracing::warn!("Failed to delete borrowed copy: {}", e);
    }

    // 6. Clean up book if no longer needed
    cleanup_orphaned_book(&db, the_copy.book_id).await;

    (
        StatusCode::OK,
        Json(json!({ "message": "Book returned successfully" })),
    )
        .into_response()
}

/// Delete a book if it has no remaining copies, is not owned, and is not in the wishlist.
async fn cleanup_orphaned_book(db: &DatabaseConnection, book_id: i32) {
    use crate::models::{book, copy};
    if let Ok(Some(bk)) = book::Entity::find_by_id(book_id).one(db).await {
        let copy_count = copy::Entity::find()
            .filter(copy::Column::BookId.eq(bk.id))
            .count(db)
            .await
            .unwrap_or(1);

        let should_delete = !bk.owned && bk.reading_status != "wanting" && copy_count == 0;

        if should_delete {
            match book::Entity::delete_by_id(bk.id).exec(db).await {
                Ok(_) => tracing::info!("Deleted orphaned book {} after loan return", bk.id),
                Err(e) => tracing::error!("Failed to delete orphaned book {}: {}", bk.id, e),
            }
        } else {
            tracing::info!(
                "Book {} kept after loan return (owned={}, reading_status='{}', copies={})",
                bk.id,
                bk.owned,
                bk.reading_status,
                copy_count
            );
        }
    }
}

#[derive(Deserialize)]
pub struct IncomingLoanRequest {
    pub from_name: String,
    pub from_url: String,
    pub library_uuid: Option<String>, // For P2P deduplication
    pub book_isbn: String,
    pub book_title: String,
}

pub async fn receive_loan_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<IncomingLoanRequest>,
) -> impl IntoResponse {
    use crate::models::p2p_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find or Create Peer (deduplicate by library_uuid if provided)
    let existing_peer = if let Some(ref uuid) = payload.library_uuid {
        // Try to find by UUID first (more stable)
        peer::Entity::find()
            .filter(peer::Column::LibraryUuid.eq(uuid))
            .one(&db)
            .await
    } else {
        // Fallback to URL matching
        peer::Entity::find()
            .filter(peer::Column::Url.eq(&payload.from_url))
            .one(&db)
            .await
    };

    let peer = match existing_peer {
        Ok(Some(mut p)) => {
            // Update URL if changed (IP might have changed)
            if p.url != payload.from_url {
                tracing::info!(
                    "📝 Updating peer {} URL: {} -> {}",
                    p.name,
                    p.url,
                    payload.from_url
                );

                // Check for conflict: Does another peer already use this new URL?
                let conflict_peer = peer::Entity::find()
                    .filter(peer::Column::Url.eq(&payload.from_url))
                    .one(&db)
                    .await
                    .unwrap_or(None);

                if let Some(conflict) = conflict_peer {
                    // If conflict is NOT the same peer (ids differ), we have a problem.
                    // Since URLs must be unique and we trust the new incoming request (it's active right now),
                    // we assume the old entry holding this IP is stale.
                    if conflict.id != p.id {
                        tracing::warn!(
                            "⚠️ Found stale peer {} holding URL {}. Deleting it.",
                            conflict.name,
                            payload.from_url
                        );
                        let _ = peer::Entity::delete_by_id(conflict.id).exec(&db).await;
                    }
                }

                let mut active: peer::ActiveModel = p.clone().into();
                active.url = Set(payload.from_url.clone());
                active.updated_at = Set(Utc::now().to_rfc3339());
                if let Ok(updated) = active.update(&db).await {
                    p = updated;
                }
            }
            p
        }
        Ok(None) => {
            // Creating new peer. Check if URL is already taken by a stale peer (since UUID didn't match)
            let conflict_peer = peer::Entity::find()
                .filter(peer::Column::Url.eq(&payload.from_url))
                .one(&db)
                .await
                .unwrap_or(None);

            if let Some(conflict) = conflict_peer {
                tracing::warn!(
                    "⚠️ New peer claims URL {} held by old peer {}. Deleting old peer.",
                    payload.from_url,
                    conflict.name
                );
                let _ = peer::Entity::delete_by_id(conflict.id).exec(&db).await;
            }

            let conn_status = if is_connection_validation_enabled(&db).await {
                "pending"
            } else {
                "accepted"
            };
            let new_peer = peer::ActiveModel {
                name: Set(payload.from_name),
                url: Set(payload.from_url),
                library_uuid: Set(payload.library_uuid),
                auto_approve: Set(conn_status == "accepted"),
                connection_status: Set(conn_status.to_string()),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
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

    // 2. Create Incoming Request
    let request_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_request::ActiveModel {
        id: Set(request_id.clone()),
        from_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Loan request received", "request_id": request_id })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save request: {}", e) })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct OutgoingLoanRequestDto {
    pub to_peer_url: String,
    pub book_isbn: String,
    pub book_title: String,
    pub request_id: Option<String>, // ID from remote peer for sync
}

pub async fn create_outgoing_request(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<OutgoingLoanRequestDto>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;
    use chrono::Utc;
    use uuid::Uuid;

    // 1. Find Peer by URL, or auto-create if not found
    let peer = match peer::Entity::find()
        .filter(peer::Column::Url.eq(&payload.to_peer_url))
        .one(&db)
        .await
    {
        Ok(Some(p)) => p,
        Ok(None) => {
            // Auto-create peer from URL (will be updated on next mDNS discovery)
            tracing::info!(
                "📝 Auto-creating peer for outgoing request: {}",
                payload.to_peer_url
            );
            let new_peer = peer::ActiveModel {
                name: Set("Réseau local".to_string()), // Placeholder name
                url: Set(payload.to_peer_url.clone()),
                auto_approve: Set(false),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
                ..Default::default()
            };
            match new_peer.insert(&db).await {
                Ok(p) => p,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Failed to create peer: {}", e) })),
                    )
                        .into_response();
                }
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

    // 2. Create Outgoing Request Log
    // Use request_id from remote peer if provided (for sync), otherwise generate new
    let request_id = payload
        .request_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let now = Utc::now().to_rfc3339();

    let new_request = p2p_outgoing_request::ActiveModel {
        id: Set(request_id),
        to_peer_id: Set(peer.id),
        book_isbn: Set(payload.book_isbn),
        book_title: Set(payload.book_title),
        status: Set("pending".to_owned()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    };

    match new_request.insert(&db).await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "message": "Outgoing request logged" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to save outgoing request: {}", e) })),
        )
            .into_response(),
    }
}

// ============ P2P LOAN CONFIRMATION ============

#[derive(Debug, Deserialize)]
pub struct LoanConfirmation {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub lender_name: String,
    pub due_date: String,
    pub request_id: Option<String>,
    /// Borrower's outgoing request ID (for precise confirmation matching)
    pub requester_request_id: Option<String>,
}

/// Receive loan confirmation from lender
/// Creates the book (if not exists) and a borrowed copy in the borrower's library
pub async fn receive_loan_confirmation(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoanConfirmation>,
) -> impl IntoResponse {
    use crate::models::p2p_outgoing_request;

    tracing::info!(
        "📚 Received loan confirmation: '{}' from {} (requester_request_id={:?})",
        payload.title,
        payload.lender_name,
        payload.requester_request_id
    );

    // Guard: verify a matching pending outgoing request exists.
    // This prevents stale relay messages from creating orphan borrowed copies.
    let has_matching_request = if let Some(ref rr_id) = payload.requester_request_id {
        // Precise match by borrower's outgoing request ID
        p2p_outgoing_request::Entity::find_by_id(rr_id)
            .filter(p2p_outgoing_request::Column::Status.eq("pending"))
            .one(&db)
            .await
            .ok()
            .flatten()
            .is_some()
    } else {
        // Backward compat: old confirmations without requester_request_id - match by ISBN
        let isbn_filter = payload.isbn.clone().unwrap_or_default();
        if !isbn_filter.is_empty() {
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                .filter(p2p_outgoing_request::Column::Status.eq("pending"))
                .one(&db)
                .await
                .ok()
                .flatten()
                .is_some()
        } else {
            // No ISBN, no requester_request_id - allow (best effort)
            true
        }
    };

    if !has_matching_request {
        tracing::warn!(
            "📚 No pending outgoing request for '{}' (requester_request_id={:?}, isbn={:?}), ignoring stale loan_confirmation",
            payload.title,
            payload.requester_request_id,
            payload.isbn
        );
        return (
            StatusCode::OK,
            Json(json!({ "message": "No pending request for this confirmation, ignored" })),
        )
            .into_response();
    }

    // Create borrowed copy via shared helper
    let params = BorrowedCopyParams {
        title: &payload.title,
        isbn: payload.isbn.as_deref(),
        author: payload.author.as_deref(),
        cover_url: payload.cover_url.as_deref(),
        lender_name: &payload.lender_name,
        due_date: &payload.due_date,
    };

    let result = match create_borrowed_copy(&db, &params).await {
        Ok(r) => r,
        Err((status, err_json)) => {
            return (status, Json(err_json)).into_response();
        }
    };

    // Update outgoing request with lender_request_id (both for idempotent and new copies)
    if let Some(ref lender_req_id) = payload.request_id {
        let outgoing = if let Some(ref rr_id) = payload.requester_request_id {
            p2p_outgoing_request::Entity::find_by_id(rr_id)
                .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                .one(&db)
                .await
                .ok()
                .flatten()
        } else {
            let isbn_filter = payload.isbn.clone().unwrap_or_default();
            p2p_outgoing_request::Entity::find()
                .filter(p2p_outgoing_request::Column::BookIsbn.eq(&isbn_filter))
                .filter(p2p_outgoing_request::Column::Status.is_in(["pending", "accepted"]))
                .one(&db)
                .await
                .ok()
                .flatten()
        };
        if let Some(outgoing) = outgoing {
            let mut active: p2p_outgoing_request::ActiveModel = outgoing.into();
            active.lender_request_id = Set(Some(lender_req_id.clone()));
            active.status = Set("accepted".to_string());
            active.updated_at = Set(Utc::now().to_rfc3339());
            if let Err(e) = active.update(&db).await {
                tracing::warn!("Failed to update outgoing request: {e}");
            }
        }
    }

    // Emit notification only for newly created copies
    if !result.already_existed {
        crate::services::notification_service::emit(
            &db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BorrowAccepted,
                title: payload.title.clone(),
                body: Some(payload.lender_name.clone()),
                ref_type: Some("peer".to_string()),
                ref_id: Some(result.copy_id.to_string()),
            },
        )
        .await;
    }

    let msg = if result.already_existed {
        "Loan already confirmed"
    } else {
        "Loan confirmed"
    };
    (
        StatusCode::OK,
        Json(json!({
            "message": msg,
            "book_id": result.book_id,
            "copy_id": result.copy_id
        })),
    )
        .into_response()
}

// ============ P2P LOAN OFFER (lender-initiated) ============

#[derive(Debug, Deserialize)]
pub struct LoanOffer {
    pub isbn: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub cover_url: Option<String>,
    pub lender_name: String,
    pub due_date: String,
    /// Lender's p2p_request ID, needed for the return flow.
    pub request_id: Option<String>,
}

/// POST /api/peers/loans/offer -- Plaintext endpoint for receiving a loan offer.
///
/// Called when a lender initiates a loan to us (no prior borrow request).
/// Unlike `receive_loan_confirmation`, this does NOT require a matching
/// `p2p_outgoing_request` since the borrower never requested the loan.
pub async fn receive_loan_offer(
    State(db): State<DatabaseConnection>,
    Json(payload): Json<LoanOffer>,
) -> impl IntoResponse {
    tracing::info!(
        "Received loan offer: '{}' from {}",
        payload.title,
        payload.lender_name
    );

    let params = BorrowedCopyParams {
        title: &payload.title,
        isbn: payload.isbn.as_deref(),
        author: payload.author.as_deref(),
        cover_url: payload.cover_url.as_deref(),
        lender_name: &payload.lender_name,
        due_date: &payload.due_date,
    };

    let result = match create_borrowed_copy(&db, &params).await {
        Ok(r) => r,
        Err((status, err_json)) => {
            return (status, Json(err_json)).into_response();
        }
    };

    // Create p2p_outgoing_request so return_borrowed_book can notify the lender
    if !result.already_existed {
        if let Some(ref lender_req_id) = payload.request_id {
            use crate::models::p2p_outgoing_request;
            let outgoing_id = uuid::Uuid::new_v4().to_string();
            let outgoing = p2p_outgoing_request::ActiveModel {
                id: Set(outgoing_id),
                to_peer_id: Set(0), // unknown in plaintext path
                book_isbn: Set(payload.isbn.clone().unwrap_or_default()),
                book_title: Set(payload.title.clone()),
                status: Set("accepted".to_string()),
                lender_request_id: Set(Some(lender_req_id.clone())),
                created_at: Set(Utc::now().to_rfc3339()),
                updated_at: Set(Utc::now().to_rfc3339()),
            };
            if let Err(e) = p2p_outgoing_request::Entity::insert(outgoing)
                .exec(&db)
                .await
            {
                tracing::warn!("Failed to create p2p_outgoing_request for loan_offer: {e}");
            }
        }

        crate::services::notification_service::emit(
            &db,
            crate::domain::CreateNotification {
                event_type: crate::domain::NotificationEventType::BorrowAccepted,
                title: payload.title.clone(),
                body: Some(payload.lender_name.clone()),
                ref_type: Some("peer".to_string()),
                ref_id: Some(result.copy_id.to_string()),
            },
        )
        .await;
    }

    let msg = if result.already_existed {
        "Loan offer already processed"
    } else {
        "Loan offer accepted"
    };
    (
        StatusCode::OK,
        Json(json!({
            "message": msg,
            "book_id": result.book_id,
            "copy_id": result.copy_id
        })),
    )
        .into_response()
}

/// Save pre-fetched books to the local peer_books cache.
///
/// Called by Flutter after loading books via relay or live WiFi fetch,
/// so the Rust backend does not need to re-fetch from the remote peer.
/// Input: { "books": [{ "id": 5, "title": "...", ... }, ...] }
pub async fn cache_books_by_id(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    // 1. Validate peer exists
    let peer = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("Peer not found: {}", peer_id) })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("DB error: {}", e) })),
            )
                .into_response();
        }
    };

    // 2. Parse books array from payload
    let books: Vec<crate::models::Book> = match payload.get("books") {
        Some(books_val) => serde_json::from_value(books_val.clone()).unwrap_or_default(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Missing 'books' field" })),
            )
                .into_response();
        }
    };

    // 3-4. Upsert books cache (preserves first_seen_at)
    let count = upsert_peer_books_cache(&db, peer.id, None, books).await;

    (
        StatusCode::OK,
        Json(json!({ "count": count, "peer_id": peer_id })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Update peer display name
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UpdatePeerDisplayNameRequest {
    pub display_name: String,
}

/// Update a peer's user-defined display name.
pub async fn update_peer_display_name(
    State(db): State<DatabaseConnection>,
    Path(peer_id): Path<i32>,
    Json(payload): Json<UpdatePeerDisplayNameRequest>,
) -> impl IntoResponse {
    let peer_opt = match peer::Entity::find_by_id(peer_id).one(&db).await {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Database error: {}", e) })),
            )
                .into_response();
        }
    };

    let peer_model = match peer_opt {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "Peer not found" })),
            )
                .into_response();
        }
    };

    let display_name = payload.display_name.trim().to_string();
    let mut active: peer::ActiveModel = peer_model.into();
    active.display_name = Set(if display_name.is_empty() {
        None
    } else {
        Some(display_name)
    });
    active.updated_at = Set(Utc::now().to_rfc3339());

    match active.update(&db).await {
        Ok(updated) => (StatusCode::OK, Json(json!({ "peer": updated }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("Failed to update display name: {}", e) })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod added_at_tests {
    use super::*;
    use crate::db;
    use crate::models::{peer, peer_book};
    use sea_orm::Set;

    async fn setup() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer(db: &DatabaseConnection) -> i32 {
        let now = chrono::Utc::now().to_rfc3339();
        let p = peer::ActiveModel {
            name: Set("test-peer".to_string()),
            url: Set("http://test-peer.local:8080".to_string()),
            last_seen: Set(Some(now.clone())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        peer::Entity::insert(p)
            .exec(db)
            .await
            .unwrap()
            .last_insert_id
    }

    async fn insert_peer_book(
        db: &DatabaseConnection,
        peer_id: i32,
        remote_book_id: i32,
        title: &str,
        added_at: Option<&str>,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        let pb = peer_book::ActiveModel {
            peer_id: Set(peer_id),
            remote_book_id: Set(remote_book_id),
            title: Set(title.to_string()),
            isbn: Set(None),
            author: Set(None),
            cover_url: Set(None),
            summary: Set(None),
            synced_at: Set(now),
            node_id: Set(None),
            first_seen_at: Set(None),
            added_at: Set(added_at.map(|s| s.to_string())),
            notified_at: Set(None),
            owned: Set(true),
            available_copies: Set(None),
            ..Default::default()
        };
        peer_book::Entity::insert(pb).exec(db).await.unwrap();
    }

    /// peer_book::Model → Book mapping must put remote_book_id into Book.id
    /// and propagate `added_at` (the owner's `books.created_at`) so cached
    /// and live responses agree on both id space and the "new" badge.
    #[tokio::test]
    async fn from_peer_book_uses_remote_id_and_added_at() {
        let pb = peer_book::Model {
            id: 999, // local row PK — must NOT be exposed as Book.id
            peer_id: 1,
            remote_book_id: 42,
            title: "Le Livre".to_string(),
            isbn: Some("978".to_string()),
            author: Some("X".to_string()),
            cover_url: None,
            summary: None,
            synced_at: "2026-04-13T00:00:00Z".to_string(),
            node_id: None,
            first_seen_at: None,
            added_at: Some("2026-04-13T08:00:00Z".to_string()),
            notified_at: None,
            owned: true,
            available_copies: Some(2),
        };
        let book: crate::models::Book = pb.into();
        assert_eq!(
            book.id,
            Some(42),
            "Book.id must be remote_book_id, not peer_book.id"
        );
        assert_eq!(book.added_at.as_deref(), Some("2026-04-13T08:00:00Z"));
        assert_eq!(book.title, "Le Livre");
        assert_eq!(book.owned, Some(true));
        assert_eq!(book.available_copies, Some(2));
    }

    /// Loan status from the owner (owned=false, available_copies=Some(0))
    /// must round-trip through the cache so the peer-lib carousel can hide
    /// non-requestable books without re-querying the peer.
    #[tokio::test]
    async fn upsert_peer_books_cache_persists_loan_status() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        let books = vec![
            crate::models::Book {
                id: Some(10),
                title: "Borrowed by peer".to_string(),
                owned: Some(false),
                available_copies: Some(1),
                ..Default::default()
            },
            crate::models::Book {
                id: Some(11),
                title: "All copies on loan".to_string(),
                owned: Some(true),
                available_copies: Some(0),
                ..Default::default()
            },
            crate::models::Book {
                id: Some(12),
                title: "Available".to_string(),
                owned: Some(true),
                available_copies: Some(2),
                ..Default::default()
            },
        ];
        upsert_peer_books_cache(&db, peer_id, None, books).await;

        let fetch = |remote_id: i32| {
            let db = db.clone();
            async move {
                peer_book::Entity::find()
                    .filter(peer_book::Column::PeerId.eq(peer_id))
                    .filter(peer_book::Column::RemoteBookId.eq(remote_id))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        let borrowed = fetch(10).await;
        let fully_lent = fetch(11).await;
        let available = fetch(12).await;

        assert!(
            !borrowed.owned,
            "peer-borrowed book must persist owned=false"
        );
        assert_eq!(borrowed.available_copies, Some(1));
        assert!(fully_lent.owned);
        assert_eq!(
            fully_lent.available_copies,
            Some(0),
            "available_copies=0 must round-trip so the carousel filter can drop fully-lent books",
        );
        assert!(available.owned);
        assert_eq!(available.available_copies, Some(2));

        // UPDATE path: a later sync marks the available book as fully lent.
        let updated = vec![crate::models::Book {
            id: Some(12),
            title: "Available".to_string(),
            owned: Some(true),
            available_copies: Some(0),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, updated).await;
        let refreshed = fetch(12).await;
        assert_eq!(
            refreshed.available_copies,
            Some(0),
            "update must refresh available_copies to reflect the current loan state",
        );
    }

    /// `redact_for_peer` strips personal fields but MUST keep `added_at`:
    /// it is editorial metadata (the owner's `books.created_at`) that
    /// drives the "new" badge for every viewer.
    #[test]
    fn redact_for_peer_preserves_added_at() {
        let mut book = crate::models::Book {
            id: Some(1),
            title: "T".to_string(),
            user_rating: Some(8),
            reading_status: Some("read".to_string()),
            added_at: Some("2026-04-15T10:00:00+00:00".to_string()),
            ..Default::default()
        };
        book.redact_for_peer();
        assert_eq!(book.user_rating, None, "rating must be redacted");
        assert_eq!(book.reading_status, None, "reading status must be redacted");
        assert_eq!(
            book.added_at.as_deref(),
            Some("2026-04-15T10:00:00+00:00"),
            "added_at is editorial metadata, not personal — must NOT be redacted",
        );
    }

    /// Upserting a previously-cached book with a fresh `added_at` from the
    /// owner must overwrite the local value (owner is source of truth).
    #[tokio::test]
    async fn upsert_peer_books_cache_refreshes_added_at() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;
        insert_peer_book(&db, peer_id, 7, "Cached", Some("2026-01-01T00:00:00+00:00")).await;

        let books = vec![crate::models::Book {
            id: Some(7),
            title: "Cached".to_string(),
            added_at: Some("2026-04-15T12:00:00+00:00".to_string()),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, books).await;

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(7))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-04-15T12:00:00+00:00"),
            "owner's added_at must overwrite the local value on upsert",
        );
    }

    /// Inserting a brand-new book via the cache upsert must persist its
    /// `added_at` as broadcast by the owner (not derived from local time).
    #[tokio::test]
    async fn upsert_peer_books_cache_persists_added_at_on_insert() {
        let db = setup().await;
        let peer_id = insert_peer(&db).await;

        let books = vec![crate::models::Book {
            id: Some(99),
            title: "New".to_string(),
            added_at: Some("2026-04-15T09:30:00+00:00".to_string()),
            ..Default::default()
        }];
        upsert_peer_books_cache(&db, peer_id, None, books).await;

        let row = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(peer_id))
            .filter(peer_book::Column::RemoteBookId.eq(99))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.added_at.as_deref(),
            Some("2026-04-15T09:30:00+00:00"),
            "owner's added_at must be persisted on first insert",
        );
    }
}

#[cfg(test)]
mod hub_catalog_cache_tests {
    use super::*;
    use crate::db;
    use crate::models::peer_book;
    use sea_orm::{ConnectionTrait, Set, Statement};

    async fn setup_cache_db() -> DatabaseConnection {
        let db = db::init_db("sqlite::memory:").await.expect("init db");
        // Directory cache uses peer_id = 0 sentinel (no matching peer row), same
        // workaround as upsert_directory_catalog_cache in frb.rs.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "PRAGMA foreign_keys = OFF".to_owned(),
        ))
        .await
        .unwrap();
        db
    }

    async fn insert_cache_entry(db: &DatabaseConnection, node_id: &str, isbn: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        let pb = peer_book::ActiveModel {
            peer_id: Set(0), // sentinel for directory entries
            remote_book_id: Set(0),
            title: Set(format!("Book {}", isbn)),
            isbn: Set(Some(isbn.to_string())),
            author: Set(None),
            cover_url: Set(None),
            summary: Set(None),
            synced_at: Set(now),
            node_id: Set(Some(node_id.to_string())),
            first_seen_at: Set(None),
            added_at: Set(None),
            notified_at: Set(None),
            ..Default::default()
        };
        peer_book::Entity::insert(pb).exec(db).await.unwrap();
    }

    /// ADR-024: purging the cache for a library_uuid must drop all sentinel
    /// directory entries for that node_id, and only those.
    #[tokio::test]
    async fn purge_hub_catalog_cache_removes_only_matching_node_id() {
        let db = setup_cache_db().await;
        let target = "41610ad0-d659-4b09-8303-faacf9e6aa36";
        let other = "26e4b4d9-acff-42cb-8b25-0bf32457a232";

        insert_cache_entry(&db, target, "978-target-1").await;
        insert_cache_entry(&db, target, "978-target-2").await;
        insert_cache_entry(&db, other, "978-other-1").await;

        purge_hub_catalog_cache(&db, target).await;

        let remaining = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(0))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "only the other node's entry should remain"
        );
        assert_eq!(remaining[0].node_id.as_deref(), Some(other));
    }

    /// Purge must be a no-op when there is nothing to remove for the given uuid.
    #[tokio::test]
    async fn purge_hub_catalog_cache_no_op_when_empty() {
        let db = setup_cache_db().await;
        let other = "26e4b4d9-acff-42cb-8b25-0bf32457a232";
        insert_cache_entry(&db, other, "978-other-1").await;

        purge_hub_catalog_cache(&db, "unknown-uuid").await;

        let remaining = peer_book::Entity::find()
            .filter(peer_book::Column::PeerId.eq(0))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
    }
}

#[cfg(test)]
mod relay_setup_tests {
    use super::*;
    use crate::db;
    use crate::services::relay_session;
    use sea_orm::{ConnectionTrait, Statement};
    use serial_test::serial;

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn seed_directory_config(db: &DatabaseConnection, token: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.execute(Statement::from_string(
            db.get_database_backend(),
            format!(
                "INSERT INTO hub_directory_config
                     (id, node_id, write_token, is_listed, requires_approval, accept_from, allow_borrowing, recovery_code, created_at, updated_at)
                 VALUES (1, 'test-node', '{token}', 0, 1, 'everyone', 1, 'rc-1', '{now}', '{now}')"
            ),
        ))
        .await
        .unwrap();
    }

    async fn directory_token(db: &DatabaseConnection) -> Option<String> {
        db.query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT write_token FROM hub_directory_config WHERE id = 1".to_owned(),
        ))
        .await
        .unwrap()
        .and_then(|row| row.try_get::<String>("", "write_token").ok())
    }

    /// Re-registering a mailbox against the **same** hub must keep the
    /// write_token in hub_directory_config intact. Before the fix, the
    /// unconditional DELETE wiped the token on every setup, causing the
    /// next profile heartbeat to hit 401 in a loop (stuck Eve scenario).
    #[tokio::test]
    #[serial]
    async fn apply_relay_setup_preserves_directory_config_when_hub_unchanged() {
        relay_session::reset_for_tests();
        let db = setup_db().await;

        apply_relay_setup(&db, "https://hub.example.org", "mbx-1", "rtok-1", "wtok-1")
            .await
            .expect("first setup");

        seed_directory_config(&db, "preserved-token").await;

        let changed =
            apply_relay_setup(&db, "https://hub.example.org/", "mbx-2", "rtok-2", "wtok-2")
                .await
                .expect("second setup same hub");

        assert!(!changed, "hub URL should be detected as unchanged");
        assert_eq!(
            directory_token(&db).await.as_deref(),
            Some("preserved-token"),
            "hub_directory_config must survive a same-URL re-setup",
        );
        assert!(
            relay_session::mailbox_created_this_session(),
            "apply_relay_setup must mark the session flag",
        );
    }

    /// A genuine hub swap still invalidates the directory config, since the
    /// write_token from the old hub cannot authenticate against the new one.
    #[tokio::test]
    #[serial]
    async fn apply_relay_setup_wipes_directory_config_when_hub_changes() {
        relay_session::reset_for_tests();
        let db = setup_db().await;

        apply_relay_setup(
            &db,
            "https://hub-a.example.org",
            "mbx-1",
            "rtok-1",
            "wtok-1",
        )
        .await
        .expect("first setup");

        seed_directory_config(&db, "stale-token").await;

        let changed = apply_relay_setup(
            &db,
            "https://hub-b.example.org",
            "mbx-2",
            "rtok-2",
            "wtok-2",
        )
        .await
        .expect("second setup new hub");

        assert!(changed, "hub URL change must be detected");
        assert!(
            directory_token(&db).await.is_none(),
            "hub_directory_config must be wiped when the hub actually changes",
        );
        assert!(
            relay_session::mailbox_created_this_session(),
            "apply_relay_setup must mark the session flag",
        );
    }

    /// First-time setup (no previous relay config) is neither a "same hub"
    /// nor a "hub change" — we simply have nothing to invalidate.
    #[tokio::test]
    #[serial]
    async fn apply_relay_setup_first_time_reports_no_change() {
        relay_session::reset_for_tests();
        let db = setup_db().await;

        assert!(
            !relay_session::mailbox_created_this_session(),
            "flag must start unset",
        );

        let changed =
            apply_relay_setup(&db, "https://hub.example.org", "mbx-1", "rtok-1", "wtok-1")
                .await
                .expect("first setup");

        assert!(!changed, "no previous hub means no change to signal");
        assert!(
            relay_session::mailbox_created_this_session(),
            "apply_relay_setup must mark the session flag",
        );
    }
}

#[cfg(test)]
mod stale_invite_flag_tests {
    //! ADR-032: `relay_write_token_invalid_at` gate + clear paths.
    use super::*;
    use crate::db;
    use sea_orm::Set;

    async fn setup_db() -> DatabaseConnection {
        db::init_db("sqlite::memory:").await.expect("init db")
    }

    async fn insert_peer_with_relay(db: &DatabaseConnection) -> peer::Model {
        let now = chrono::Utc::now().to_rfc3339();
        let active = peer::ActiveModel {
            name: Set("test-peer".to_string()),
            url: Set("http://test-peer.local:8080".to_string()),
            relay_url: Set(Some("https://hub.example.org".to_string())),
            mailbox_id: Set(Some("mbx-original".to_string())),
            relay_write_token: Set(Some("wtok-original".to_string())),
            relay_write_token_invalid_at: Set(None),
            key_exchange_done: Set(true),
            connection_status: Set("accepted".to_string()),
            created_at: Set(now.clone()),
            updated_at: Set(now),
            ..Default::default()
        };
        let id = peer::Entity::insert(active)
            .exec(db)
            .await
            .expect("insert peer")
            .last_insert_id;
        peer::Entity::find_by_id(id)
            .one(db)
            .await
            .expect("query")
            .expect("peer row")
    }

    /// `mark_peer_invite_stale` persists a non-empty timestamp so later
    /// queries see the gate closed.
    #[tokio::test]
    async fn mark_peer_invite_stale_sets_timestamp() {
        let db = setup_db().await;
        let p = insert_peer_with_relay(&db).await;
        assert!(p.relay_write_token_invalid_at.is_none());

        mark_peer_invite_stale(&db, p.id).await;

        let reloaded = peer::Entity::find_by_id(p.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.relay_write_token_invalid_at.is_some(),
            "timestamp must be persisted after mark_peer_invite_stale"
        );
        assert!(
            !reloaded.relay_gate_allows_send(),
            "fresh flag must close the retry gate"
        );
    }

    /// Gate admits a send again after the retry window has elapsed, so a
    /// peer coming back online eventually recovers without user action.
    #[tokio::test]
    async fn gate_admits_send_after_retry_window() {
        let db = setup_db().await;
        let mut p = insert_peer_with_relay(&db).await;

        let one_hour_ago = (chrono::Utc::now() - chrono::Duration::seconds(3601)).to_rfc3339();
        p.relay_write_token_invalid_at = Some(one_hour_ago);
        assert!(
            p.relay_gate_allows_send(),
            "gate must admit a send once the retry window has elapsed"
        );

        p.relay_write_token_invalid_at = Some(chrono::Utc::now().to_rfc3339());
        assert!(
            !p.relay_gate_allows_send(),
            "gate must close again when the timestamp is fresh"
        );
    }

    /// `refresh_peer_relay_credentials` shouldn't be callable here without a
    /// real peer HTTP endpoint, so we simulate the credential write path: a
    /// successful update must clear any previously set stale-invite flag.
    #[tokio::test]
    async fn refresh_clears_stale_invite_flag() {
        let db = setup_db().await;
        let p = insert_peer_with_relay(&db).await;
        mark_peer_invite_stale(&db, p.id).await;

        // Emulate the credential-write branch inside refresh_peer_relay_credentials
        let existing = peer::Entity::find_by_id(p.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut active: peer::ActiveModel = existing.into();
        active.relay_write_token = Set(Some("wtok-fresh".to_string()));
        active.relay_write_token_invalid_at = Set(None);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(&db).await.expect("update peer");

        let reloaded = peer::Entity::find_by_id(p.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reloaded.relay_write_token_invalid_at.is_none(),
            "refresh must clear stale-invite flag"
        );
        assert!(reloaded.relay_gate_allows_send());
    }
}
