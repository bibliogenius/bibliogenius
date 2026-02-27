// FFI API module for flutter_rust_bridge
// This module exposes core functionality to Flutter without HTTP layer
//
// ARCHITECTURE: This module provides direct database access for all native platforms.
// Web uses WASM (future). All native platforms use FFI for local-first operation.

use flutter_rust_bridge::frb;
use sea_orm::DatabaseConnection;
use std::sync::OnceLock;
use tokio::runtime::Runtime;
use tower_http::cors::{Any, CorsLayer};

// Global database connection (initialized once on app start)
static DB: OnceLock<DatabaseConnection> = OnceLock::new();
#[allow(dead_code)]
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Get or create the tokio runtime
/// Uses current_thread runtime for iOS/mobile compatibility
#[allow(dead_code)]
fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        // Use current_thread runtime for mobile FFI compatibility
        // Multi-threaded runtime can cause issues on iOS
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|e| {
                eprintln!("FFI: Failed to create Tokio runtime: {}", e);
                // Create a minimal runtime as fallback
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("Failed to create even minimal Tokio runtime")
            })
    })
}

/// Install a panic hook to prevent crashes on iOS
/// This converts panics into logs instead of aborting
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|panic_info| {
            let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            let location = panic_info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown location".to_string());
            eprintln!("FFI PANIC at {}: {}", location, message);
        }));
    });
}

/// Get the database connection (must be initialized first)
fn db() -> Option<&'static DatabaseConnection> {
    DB.get()
}

/// Load the Google Books API key from the installation profile.
async fn load_google_books_api_key() -> Option<String> {
    use crate::models::installation_profile::Entity as ProfileEntity;
    use sea_orm::EntityTrait;

    let db = db()?;
    if let Ok(Some(profile)) = ProfileEntity::find_by_id(1).one(db).await {
        let api_keys: std::collections::HashMap<String, String> = profile
            .api_keys
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        return api_keys.get("google_books").cloned();
    }
    None
}

// ============ FFI-Compatible Data Structures ============

/// Simplified book structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbBook {
    pub id: Option<i32>,
    pub title: String,
    pub author: Option<String>,
    pub isbn: Option<String>,
    pub summary: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<i32>,
    pub cover_url: Option<String>,
    pub large_cover_url: Option<String>,
    pub reading_status: Option<String>,
    pub shelf_position: Option<i32>,
    pub user_rating: Option<i32>,
    pub subjects: Option<String>, // JSON array as string
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub finished_reading_at: Option<String>,
    pub started_reading_at: Option<String>,
    pub owned: bool,        // Added for copy management
    pub price: Option<f64>, // Added for bookseller profile
    pub digital_formats: Option<Vec<String>>,
}

/// Convert domain Book to FFI-safe FrbBook
impl From<crate::models::Book> for FrbBook {
    fn from(book: crate::models::Book) -> Self {
        FrbBook {
            id: book.id,
            title: book.title,
            author: book.author,
            isbn: book.isbn,
            summary: book.summary,
            publisher: book.publisher,
            publication_year: book.publication_year,
            cover_url: book.cover_url,
            large_cover_url: book.large_cover_url,
            reading_status: book.reading_status,
            shelf_position: book.shelf_position,
            user_rating: book.user_rating,
            subjects: book
                .subjects
                .map(|s| serde_json::to_string(&s).unwrap_or_default()),
            created_at: None, // Not available in Book DTO
            updated_at: None, // Not available in Book DTO
            finished_reading_at: book.finished_reading_at.flatten(),
            started_reading_at: book.started_reading_at.flatten(),
            owned: book.owned.unwrap_or(true), // Default to owned if None (legacy/missing)
            price: book.price,
            digital_formats: book.digital_formats,
        }
    }
}

// ============ Initialization ============

/// Initialize the FFI backend with database at the given path
/// Must be called before any other FFI functions
pub async fn init_backend(db_path: String) -> Result<String, String> {
    // Install panic hook first thing to catch any panics
    install_panic_hook();

    if DB.get().is_some() {
        return Ok("Already initialized".to_string());
    }

    let db_url = format!("sqlite:{}?mode=rwc", db_path);

    // Set the DATABASE_URL environment variable so that other components (like MCP config)
    // can access the correct database path being used by the FFI instance.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("DATABASE_URL", &db_url) };
    tracing::info!("FFI: Set DATABASE_URL env var to: {}", db_url);

    match crate::db::init_db(&db_url).await {
        Ok(conn) => match DB.set(conn) {
            Ok(_) => Ok("Backend initialized successfully".to_string()),
            Err(_) => Err("Failed to set database connection".to_string()),
        },
        Err(e) => Err(format!("Database initialization failed: {}", e)),
    }
}

// ============ Health Check ============

/// Check if the FFI backend is healthy
#[frb(sync)]
pub fn health_check() -> String {
    if DB.get().is_some() {
        "OK".to_string()
    } else {
        "NOT_INITIALIZED".to_string()
    }
}

/// Get the FFI backend version
#[frb(sync)]
pub fn get_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Simple greeting function to test the bridge
#[frb(sync)]
pub fn greet(name: String) -> String {
    format!("Hello, {}! Welcome to BiblioGenius.", name)
}

// ============ mDNS Local Discovery (FFI) ============

/// Discovered peer on local network (FFI-compatible)
#[frb(dart_metadata=("freezed"))]
pub struct FrbDiscoveredPeer {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub addresses: Vec<String>,
    pub library_id: Option<String>,
    pub ed25519_public_key: Option<String>,
    pub x25519_public_key: Option<String>,
    pub discovered_at: String,
}

impl From<crate::services::mdns::DiscoveredPeer> for FrbDiscoveredPeer {
    fn from(peer: crate::services::mdns::DiscoveredPeer) -> Self {
        FrbDiscoveredPeer {
            name: peer.name,
            host: peer.host,
            port: peer.port,
            addresses: peer.addresses,
            library_id: peer.library_id,
            ed25519_public_key: peer.ed25519_public_key,
            x25519_public_key: peer.x25519_public_key,
            discovered_at: peer.discovered_at,
        }
    }
}

/// Check if mDNS discovery service is currently active
/// This is a sync function that can be called to check status
#[frb(sync)]
pub fn is_mdns_available() -> bool {
    crate::services::mdns::is_mdns_active()
}

/// Get the mDNS service type used for discovery
#[frb(sync)]
pub fn get_mdns_service_type() -> String {
    "_bibliogenius._tcp.local.".to_string()
}

/// Get locally discovered peers via mDNS
/// This returns peers that have been found on the local network
pub async fn get_local_peers_ffi() -> Result<Vec<FrbDiscoveredPeer>, String> {
    let peers = crate::services::mdns::get_local_peers();
    tracing::info!(
        "🔍 mDNS FFI: get_local_peers_ffi returning {} peers",
        peers.len()
    );
    for peer in &peers {
        tracing::info!(
            "  📚 Peer: {} at {:?}:{}",
            peer.name,
            peer.addresses.first(),
            peer.port
        );
    }
    Ok(peers.into_iter().map(FrbDiscoveredPeer::from).collect())
}

/// Initialize mDNS service for discovery
/// Must be called to start announcing and discovering peers
pub async fn init_mdns_ffi(
    library_name: String,
    port: u16,
    library_id: Option<String>,
    ed25519_public_key: Option<String>,
    x25519_public_key: Option<String>,
) -> Result<String, String> {
    tracing::info!(
        "mDNS FFI: init_mdns_ffi called with name='{}', port={}, has_keys={}",
        library_name,
        port,
        ed25519_public_key.is_some()
    );

    match crate::services::mdns::init_mdns(
        &library_name,
        port,
        library_id,
        ed25519_public_key,
        x25519_public_key,
    ) {
        Ok(_) => {
            tracing::info!("mDNS FFI: Service started successfully");
            Ok("mDNS service started".to_string())
        }
        Err(e) => {
            tracing::error!("mDNS FFI: Failed to start - {}", e);
            Err(e.to_string())
        }
    }
}

/// Stop mDNS service
pub async fn stop_mdns_ffi() -> Result<String, String> {
    crate::services::mdns::stop_mdns();
    Ok("mDNS service stopped".to_string())
}

// ============ E2EE Identity & Key Exchange (FFI) ============

/// Global identity service (initialized once, similar to DB)
static IDENTITY_SERVICE: OnceLock<crate::services::IdentityService> = OnceLock::new();

/// Initialize the node's cryptographic identity.
/// Must be called after init_backend and after obtaining the library UUID.
/// Uses Argon2(library_uuid) to encrypt/decrypt the stored keypair.
pub async fn init_identity_ffi(library_uuid: String) -> Result<String, String> {
    let db_conn = db().ok_or("Database not initialized")?;

    let svc =
        IDENTITY_SERVICE.get_or_init(|| crate::services::IdentityService::new(db_conn.clone()));

    svc.init(&library_uuid).await?;
    Ok("Identity initialized".to_string())
}

/// Get the node's public keys as JSON: {"ed25519": "hex...", "x25519": "hex..."}
pub async fn get_public_keys_ffi() -> Result<String, String> {
    let svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;

    let (ed25519, x25519) = svc.get_public_keys_hex()?;
    Ok(serde_json::json!({
        "ed25519": ed25519,
        "x25519": x25519,
    })
    .to_string())
}

/// Generate a QR v2 payload as JSON string.
/// Includes library name, URL, UUID, and public keys.
pub async fn generate_qr_payload_ffi(
    library_name: String,
    url: String,
    library_uuid: String,
) -> Result<String, String> {
    let svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;

    let (ed25519, x25519) = svc.get_public_keys_hex()?;

    let payload = serde_json::json!({
        "version": 2,
        "name": library_name,
        "url": url,
        "library_uuid": library_uuid,
        "ed25519_public_key": ed25519,
        "x25519_public_key": x25519,
    });

    Ok(payload.to_string())
}

/// Parse a QR payload (supports both v1 and v2 formats).
/// Returns a normalized JSON string with all available fields.
pub async fn parse_qr_payload_ffi(payload: String) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(&payload).map_err(|e| format!("Invalid QR JSON: {e}"))?;

    // Check for version field to determine format
    let version = parsed.get("version").and_then(|v| v.as_i64()).unwrap_or(1);

    let result = if version >= 2 {
        // QR v2: full payload with keys
        parsed
    } else {
        // QR v1: legacy format with just name + url
        serde_json::json!({
            "version": 1,
            "name": parsed.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "url": parsed.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        })
    };

    Ok(result.to_string())
}

/// Generate an invite link with the library's connection info encoded in the URL fragment.
/// Format: https://bibliogenius.org/invite#BASE64URL(json)
/// The fragment (#) is never sent to the web server (B8 compliance).
/// Payload v3 adds optional relay info for WAN connectivity.
pub async fn generate_invite_link_ffi(
    library_name: String,
    url: String,
    library_uuid: String,
    relay_url: Option<String>,
    mailbox_id: Option<String>,
    relay_write_token: Option<String>,
) -> Result<String, String> {
    use base64::Engine;

    let svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;

    let (ed25519, x25519) = svc.get_public_keys_hex()?;

    let mut payload = serde_json::json!({
        "version": 3,
        "name": library_name,
        "url": url,
        "library_uuid": library_uuid,
        "ed25519_public_key": ed25519,
        "x25519_public_key": x25519,
    });

    // Include relay info if available (for WAN connectivity)
    if let Some(ref r) = relay_url {
        payload["relay_url"] = serde_json::Value::String(r.clone());
    }
    if let Some(ref m) = mailbox_id {
        payload["mailbox_id"] = serde_json::Value::String(m.clone());
    }
    if let Some(ref t) = relay_write_token {
        payload["relay_write_token"] = serde_json::Value::String(t.clone());
    }

    let json_bytes = payload.to_string().into_bytes();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json_bytes);

    Ok(format!("https://bibliogenius.org/invite#{encoded}"))
}

/// Parse an invite link, extracting the JSON payload from the URL fragment.
pub async fn parse_invite_link_ffi(link: String) -> Result<String, String> {
    use base64::Engine;

    let fragment = link
        .split_once('#')
        .map(|(_, f)| f)
        .ok_or("Invalid invite link: no fragment")?;

    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(fragment)
        .map_err(|e| format!("Invalid base64 in invite link: {e}"))?;

    let json_str =
        String::from_utf8(decoded).map_err(|e| format!("Invalid UTF-8 in invite link: {e}"))?;

    // Validate it's valid JSON
    let _: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| format!("Invalid JSON in invite: {e}"))?;

    // Re-parse to normalize (same as QR parse for consistency)
    parse_qr_payload_ffi(json_str).await
}

// ============ Initializers & Converters ============

impl From<FrbBook> for crate::models::Book {
    fn from(frb_book: FrbBook) -> Self {
        let subjects: Option<Vec<String>> = frb_book
            .subjects
            .and_then(|s| serde_json::from_str(&s).ok());

        crate::models::Book {
            id: frb_book.id,
            title: frb_book.title,
            isbn: frb_book.isbn,
            summary: frb_book.summary,
            publisher: frb_book.publisher,
            publication_year: frb_book.publication_year,
            subjects,
            reading_status: frb_book.reading_status,
            user_rating: frb_book.user_rating,
            shelf_position: frb_book.shelf_position,
            author: frb_book.author.clone(),
            authors: frb_book.author.map(|a| vec![a]), // Convert single author to array
            cover_url: frb_book.cover_url,
            large_cover_url: frb_book.large_cover_url,
            // Default other fields
            dewey_decimal: None,
            lcc: None,
            marc_record: None,
            cataloguing_notes: None,
            source_data: None,
            finished_reading_at: frb_book.finished_reading_at.map(Some),
            started_reading_at: frb_book.started_reading_at.map(Some),
            source: None,
            owned: Some(frb_book.owned),
            price: frb_book.price, // Price now exposed in FFI layer
            language: None,
            digital_formats: frb_book.digital_formats,
        }
    }
}

// ============ Books API ============

/// Create a new book
pub async fn create_book(book: FrbBook) -> Result<FrbBook, String> {
    println!("DEBUG FFI: create_book received: {:?}", book.title);
    if let Some(ref isbn) = book.isbn {
        println!("DEBUG FFI: create_book received ISBN: {}", isbn);
    } else {
        println!("DEBUG FFI: create_book received NO ISBN");
    }
    let db = db().ok_or("Database not initialized")?;
    let book_dto: crate::models::Book = book.into();

    match crate::services::book_service::create_book(db, book_dto).await {
        Ok(created_book) => {
            // Check achievements after book creation (e.g. first_book, collector badges)
            let _ = {
                let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
                let game_repo =
                    crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
                let puzzle_repo =
                    crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(
                        db.clone(),
                    );
                crate::services::gamification_service::check_and_unlock_achievements(
                    &gamification_repo,
                    &game_repo,
                    Some(&puzzle_repo),
                )
                .await
            };
            Ok(FrbBook::from(created_book))
        }
        Err(crate::services::book_service::ServiceError::InvalidInput(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Get all books with optional filters
pub async fn get_all_books(
    status: Option<String>,
    title: Option<String>,
    tag: Option<String>,
) -> Result<Vec<FrbBook>, String> {
    let db = db().ok_or("Database not initialized")?;

    let filter = crate::services::book_service::BookFilter {
        status,
        title,
        tag,
        author: None,
    };

    match crate::services::book_service::list_books(db, filter).await {
        Ok(books) => Ok(books.into_iter().map(FrbBook::from).collect()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Get a single book by ID
pub async fn get_book_by_id(id: i32) -> Result<FrbBook, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::book_service::get_book(db, id).await {
        Ok(book) => {
            println!(
                "DEBUG FFI get_book_by_id({}): cover_url={:?}",
                id, book.cover_url
            );
            Ok(FrbBook::from(book))
        }
        Err(crate::services::book_service::ServiceError::NotFound) => {
            Err("Book not found".to_string())
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Update an existing book
pub async fn update_book(id: i32, book: FrbBook) -> Result<FrbBook, String> {
    let db = db().ok_or("Database not initialized")?;
    let book_dto: crate::models::Book = book.into();

    match crate::services::book_service::update_book(db, id, book_dto).await {
        Ok(updated_book) => Ok(FrbBook::from(updated_book)),
        Err(crate::services::book_service::ServiceError::InvalidInput(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Delete a book
pub async fn delete_book(id: i32) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::book_service::delete_book(db, id).await {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count total books
pub async fn count_books() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::book_service::count_books(db).await {
        Ok(count) => Ok(count),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Enrich books that have an ISBN but no cover by checking external sources.
/// Runs in background, returns the count of covers found and persisted.
pub async fn enrich_missing_covers() -> Result<i32, String> {
    let db = db().ok_or("Database not initialized")?;
    let book_repo =
        crate::infrastructure::repositories::book_repository::SeaOrmBookRepository::new(db.clone());
    crate::services::book_service::enrich_missing_covers(db, &book_repo)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Search for a cover URL for a single ISBN from external sources.
pub async fn search_cover_for_book(isbn: String) -> Result<Option<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_service::search_cover_for_book(db, &isbn)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Search for a cover URL by title with author verification.
/// Used as a fallback when ISBN-based search returns nothing.
/// Returns a cover only if the result author matches the given author.
pub async fn search_cover_by_title(
    title: String,
    author: Option<String>,
    enable_google: Option<bool>,
) -> Result<Option<String>, String> {
    let gb_api_key = load_google_books_api_key().await;
    crate::services::book_service::search_cover_by_title(
        &title,
        author.as_deref(),
        enable_google.unwrap_or(false),
        gb_api_key.as_deref(),
    )
    .await
    .map_err(|e| format!("{:?}", e))
}

/// A cover candidate from an external source, for the multi-cover picker.
#[frb(dart_metadata=("freezed"))]
pub struct FrbCoverCandidate {
    pub url: String,
    pub source: String,
}

impl From<crate::services::book_service::CoverCandidate> for FrbCoverCandidate {
    fn from(c: crate::services::book_service::CoverCandidate) -> Self {
        FrbCoverCandidate {
            url: c.url,
            source: c.source,
        }
    }
}

/// Search ALL enabled cover sources in parallel for a given ISBN.
/// Returns all found cover candidates for the picker carousel.
pub async fn search_all_covers_for_book(isbn: String) -> Result<Vec<FrbCoverCandidate>, String> {
    let db = db().ok_or("Database not initialized")?;
    crate::services::book_service::search_all_covers_for_book(db, &isbn)
        .await
        .map(|v| v.into_iter().map(FrbCoverCandidate::from).collect())
        .map_err(|e| format!("{:?}", e))
}

/// Search ALL enabled sources by title in parallel for the cover picker.
pub async fn search_all_covers_by_title(
    title: String,
    author: Option<String>,
    enable_google: Option<bool>,
) -> Result<Vec<FrbCoverCandidate>, String> {
    let gb_api_key = load_google_books_api_key().await;
    crate::services::book_service::search_all_covers_by_title(
        &title,
        author.as_deref(),
        enable_google.unwrap_or(false),
        gb_api_key.as_deref(),
    )
    .await
    .map(|v| v.into_iter().map(FrbCoverCandidate::from).collect())
    .map_err(|e| format!("{:?}", e))
}

/// Metadata fetched from external sources for a book refresh.
/// Each field is optional — only non-null fields have data from the source.
#[frb(dart_metadata=("freezed"))]
pub struct FrbBookMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub publisher: Option<String>,
    pub publication_year: Option<String>,
    pub cover_url: Option<String>,
    pub summary: Option<String>,
}

/// Look up book metadata by ISBN from external sources (BNF, Inventaire, OpenLibrary, etc.).
/// Used by the metadata refresh feature to let users preview and cherry-pick fields.
pub async fn lookup_book_metadata(
    isbn: String,
    lang: Option<String>,
) -> Result<Option<FrbBookMetadata>, String> {
    let db = db().ok_or("Database not initialized")?;
    let result =
        crate::services::lookup_service::lookup_metadata_by_isbn(db, &isbn, lang.as_deref())
            .await?;
    Ok(result.map(|m| FrbBookMetadata {
        title: Some(m.title),
        author: if m.authors.is_empty() {
            None
        } else {
            Some(
                m.authors
                    .iter()
                    .map(|a| a.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        },
        publisher: m.publisher,
        publication_year: m.publication_year,
        cover_url: m.cover_url,
        summary: m.summary,
    }))
}

/// Simplified tag structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbTag {
    pub id: i32,
    pub name: String,
    pub parent_id: Option<i32>,
    pub count: i64,
}

/// Get all tags with hierarchy info
pub async fn get_all_tags() -> Result<Vec<FrbTag>, String> {
    let db = db().ok_or("Database not initialized")?;

    // 1. Fetch hierarchical tags from DB
    use crate::models::tag;
    use sea_orm::{EntityTrait, QueryOrder};
    let db_tags = tag::Entity::find()
        .order_by_asc(tag::Column::Name)
        .all(db)
        .await
        .map_err(|e| format!("{:?}", e))?;

    // 2. Fetch counts from legacy book subjects (JSON)
    // We reuse the logic from `list_tags` because `book_tags` table might be empty
    let books = crate::models::book::Entity::find()
        .all(db)
        .await
        .map_err(|e| format!("{:?}", e))?;

    let mut tag_counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for book in books {
        if let Some(subjects_json) = book.subjects
            && let Ok(subjects) = serde_json::from_str::<Vec<String>>(&subjects_json)
        {
            for subject in subjects {
                if !subject.trim().is_empty() {
                    *tag_counts.entry(subject.trim().to_string()).or_insert(0) += 1;
                }
            }
        }
    }

    // 3. Merge: Prioritize DB hierarchy, add legacy tags as roots if missing
    let mut result = Vec::new();
    let mut processed_names = std::collections::HashSet::new();

    // Add DB tags
    for t in db_tags {
        let count = *tag_counts.get(&t.name).unwrap_or(&0);
        processed_names.insert(t.name.clone());
        result.push(FrbTag {
            id: t.id,
            name: t.name,
            parent_id: t.parent_id,
            count,
        });
    }

    // Add remaining legacy tags (as orphans)
    // Give them negative IDs to distinguish from DB tags (which are positive)
    let mut next_legacy_id = -1;
    for (name, count) in tag_counts {
        if !processed_names.contains(&name) {
            result.push(FrbTag {
                id: next_legacy_id,
                name,
                parent_id: None,
                count,
            });
            next_legacy_id -= 1;
        }
    }

    // Sort by name
    result.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(result)
}

/// Create a new tag
pub async fn create_tag(name: String, parent_id: Option<i32>) -> Result<FrbTag, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::models::tag;
    use sea_orm::{ActiveModelTrait, Set};

    let new_tag = tag::ActiveModel {
        name: Set(name),
        parent_id: Set(parent_id),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        updated_at: Set(chrono::Utc::now().to_rfc3339()),
        ..Default::default()
    };

    match new_tag.insert(db).await {
        Ok(t) => {
            let _ = crate::sync::log_operation(db, "tag", t.id, "INSERT", None).await;
            Ok(FrbTag {
                id: t.id,
                name: t.name,
                parent_id: t.parent_id,
                count: 0,
            })
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Update a tag
pub async fn update_tag(id: i32, name: String, parent_id: Option<i32>) -> Result<FrbTag, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::models::tag;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    let tag = tag::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| format!("{:?}", e))?;
    let Some(tag) = tag else {
        return Err("Tag not found".to_string());
    };

    let mut active: tag::ActiveModel = tag.into();
    active.name = Set(name);
    active.parent_id = Set(parent_id);
    active.updated_at = Set(chrono::Utc::now().to_rfc3339());

    match active.update(db).await {
        Ok(t) => {
            let _ = crate::sync::log_operation(db, "tag", t.id, "UPDATE", None).await;
            Ok(FrbTag {
                id: t.id,
                name: t.name,
                parent_id: t.parent_id,
                count: 0,
            })
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Delete a tag
pub async fn delete_tag(id: i32) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::models::tag;
    use sea_orm::EntityTrait;

    match tag::Entity::delete_by_id(id).exec(db).await {
        Ok(_) => {
            let _ = crate::sync::log_operation(db, "tag", id, "DELETE", None).await;
            Ok(())
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Simplified contact structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbContact {
    pub id: Option<i32>,
    pub contact_type: String,
    pub name: String,
    pub first_name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address: Option<String>,
    pub street_address: Option<String>,
    pub postal_code: Option<String>,
    pub city: Option<String>,
    pub country: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub notes: Option<String>,
    pub user_id: Option<i32>,
    pub library_owner_id: Option<i32>,
    pub is_active: bool,
}

impl From<crate::services::contact_service::ContactDto> for FrbContact {
    fn from(c: crate::services::contact_service::ContactDto) -> Self {
        FrbContact {
            id: c.id,
            contact_type: c.contact_type,
            name: c.name,
            first_name: c.first_name,
            email: c.email,
            phone: c.phone,
            address: c.address,
            street_address: c.street_address,
            postal_code: c.postal_code,
            city: c.city,
            country: c.country,
            latitude: c.latitude,
            longitude: c.longitude,
            notes: c.notes,
            user_id: c.user_id,
            library_owner_id: c.library_owner_id,
            is_active: c.is_active,
        }
    }
}

/// Reorder books by updating shelf positions
pub async fn reorder_books(book_ids: Vec<i32>) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;

    // In a real app, this should be transactional.
    // For now, we just iterate and update.
    for (index, book_id) in book_ids.iter().enumerate() {
        use sea_orm::{ActiveModelTrait, EntityTrait, Set};
        match crate::models::book::Entity::find_by_id(*book_id)
            .one(db)
            .await
        {
            Ok(Some(book)) => {
                let mut active: crate::models::book::ActiveModel = book.into();
                active.shelf_position = Set(Some(index as i32));
                let _ = active.update(db).await;
            }
            _ => continue,
        }
    }
    Ok(())
}

// ============ Contacts API ============

/// Get all contacts with optional filters
pub async fn get_all_contacts(
    library_id: Option<i32>,
    contact_type: Option<String>,
) -> Result<Vec<FrbContact>, String> {
    let db = db().ok_or("Database not initialized")?;

    let filter = crate::services::contact_service::ContactFilter {
        library_id,
        contact_type,
    };

    match crate::services::contact_service::list_contacts(db, filter).await {
        Ok(contacts) => Ok(contacts.into_iter().map(FrbContact::from).collect()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Get a single contact by ID
pub async fn get_contact_by_id(id: i32) -> Result<FrbContact, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::contact_service::get_contact(db, id).await {
        Ok(contact) => Ok(FrbContact::from(contact)),
        Err(crate::services::contact_service::ServiceError::NotFound) => {
            Err("Contact not found".to_string())
        }
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count total contacts
pub async fn count_contacts() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::contact_service::count_contacts(db).await {
        Ok(count) => Ok(count),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Create a new contact
pub async fn create_contact(contact: FrbContact) -> Result<FrbContact, String> {
    let db = db().ok_or("Database not initialized")?;

    // Convert FrbContact to ContactDto for the service layer
    let dto = crate::services::contact_service::ContactDto {
        id: None,
        contact_type: contact.contact_type,
        name: contact.name,
        first_name: contact.first_name,
        email: contact.email,
        phone: contact.phone,
        address: contact.address,
        street_address: contact.street_address,
        postal_code: contact.postal_code,
        city: contact.city,
        country: contact.country,
        latitude: contact.latitude,
        longitude: contact.longitude,
        notes: contact.notes,
        user_id: contact.user_id,
        library_owner_id: contact.library_owner_id.or(Some(1)), // Fallback to 1 if not provided
        is_active: contact.is_active,
    };

    match crate::services::contact_service::create_contact(db, dto).await {
        Ok(created) => Ok(FrbContact::from(created)),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Update an existing contact
pub async fn update_contact(contact: FrbContact) -> Result<FrbContact, String> {
    let db = db().ok_or("Database not initialized")?;

    // Convert FrbContact to ContactDto for the service layer
    let dto = crate::services::contact_service::ContactDto {
        id: contact.id,
        contact_type: contact.contact_type,
        name: contact.name,
        first_name: contact.first_name,
        email: contact.email,
        phone: contact.phone,
        address: contact.address,
        street_address: contact.street_address,
        postal_code: contact.postal_code,
        city: contact.city,
        country: contact.country,
        latitude: contact.latitude,
        longitude: contact.longitude,
        notes: contact.notes,
        user_id: contact.user_id,
        library_owner_id: contact.library_owner_id.or(Some(1)),
        is_active: contact.is_active,
    };

    match crate::services::contact_service::update_contact(db, dto).await {
        Ok(updated) => Ok(FrbContact::from(updated)),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Delete a contact by ID (soft delete)
pub async fn delete_contact(id: i32) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    match crate::services::contact_service::delete_contact(db, id).await {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

// ============ Loans API ============

/// Simplified loan structure for FFI
#[frb(dart_metadata=("freezed"))]
pub struct FrbLoan {
    pub id: i32,
    pub copy_id: i32,
    pub contact_id: i32,
    pub library_id: i32,
    pub loan_date: String,
    pub due_date: String,
    pub return_date: Option<String>,
    pub status: String,
    pub notes: Option<String>,
    pub contact_name: String,
    pub book_title: String,
}

impl From<crate::services::loan_service::LoanWithDetails> for FrbLoan {
    fn from(l: crate::services::loan_service::LoanWithDetails) -> Self {
        FrbLoan {
            id: l.id,
            copy_id: l.copy_id,
            contact_id: l.contact_id,
            library_id: l.library_id,
            loan_date: l.loan_date,
            due_date: l.due_date,
            return_date: l.return_date,
            status: l.status,
            notes: l.notes,
            contact_name: l.contact_name,
            book_title: l.book_title,
        }
    }
}

/// Get all loans with optional filters
pub async fn get_all_loans(
    library_id: Option<i32>,
    status: Option<String>,
    contact_id: Option<i32>,
) -> Result<Vec<FrbLoan>, String> {
    let db = db().ok_or("Database not initialized")?;

    let filter = crate::services::loan_service::LoanFilter {
        library_id,
        status,
        contact_id,
    };

    match crate::services::loan_service::list_loans(db, filter).await {
        Ok(loans) => Ok(loans.into_iter().map(FrbLoan::from).collect()),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count active loans
pub async fn count_active_loans() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::loan_service::count_active_loans(db).await {
        Ok(count) => Ok(count),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Create a new loan
pub async fn create_loan(
    copy_id: i32,
    contact_id: i32,
    library_id: i32,
    loan_date: String,
    due_date: String,
    notes: Option<String>,
) -> Result<i32, String> {
    let db = db().ok_or("Database not initialized")?;

    let dto = crate::models::loan::LoanDto {
        id: None,
        copy_id,
        contact_id,
        library_id,
        loan_date,
        due_date,
        return_date: None,
        status: None,
        notes,
    };

    match crate::services::loan_service::create_loan(db, dto).await {
        Ok(loan) => Ok(loan.id),
        Err(crate::services::loan_service::ServiceError::NotFound) => {
            Err("Copy not found".to_string())
        }
        Err(crate::services::loan_service::ServiceError::InvalidState(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Count returned loans (for cleanup confirmation dialog)
pub async fn count_returned_loans() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::count_returned_loans(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Delete all returned loans, returns the number of deleted rows
pub async fn delete_returned_loans() -> Result<u64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::delete_returned_loans(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Count closed incoming P2P requests (not pending)
pub async fn count_closed_incoming_requests() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::count_closed_incoming_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Delete all closed incoming P2P requests (not pending)
pub async fn delete_closed_incoming_requests() -> Result<u64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::delete_closed_incoming_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Count closed outgoing P2P requests (not pending)
pub async fn count_closed_outgoing_requests() -> Result<i64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::count_closed_outgoing_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Delete all closed outgoing P2P requests (not pending)
pub async fn delete_closed_outgoing_requests() -> Result<u64, String> {
    let db = db().ok_or("Database not initialized")?;

    crate::services::loan_service::delete_closed_outgoing_requests(db)
        .await
        .map_err(|e| format!("{:?}", e))
}

/// Return a loan
pub async fn return_loan(id: i32) -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;

    match crate::services::loan_service::return_loan(db, id).await {
        Ok(_) => Ok("Loan returned successfully".to_string()),
        Err(crate::services::loan_service::ServiceError::NotFound) => {
            Err("Loan not found".to_string())
        }
        Err(crate::services::loan_service::ServiceError::InvalidState(msg)) => Err(msg),
        Err(e) => Err(format!("{:?}", e)),
    }
}

// ============ Reset API ============

/// Reset the entire application - deletes all data from all tables
/// This is irreversible and should be used with caution
pub async fn reset_app() -> Result<String, String> {
    let db = db().ok_or("Database not initialized")?;

    use crate::models::{
        author, book, book_authors, book_tags, collection, collection_book, contact, copy,
        installation_profile, library, library_config, loan, operation_log, p2p_outgoing_request,
        p2p_request, peer, peer_book, tag, user,
    };
    use sea_orm::EntityTrait;

    // Helper macro to delete all from a table
    macro_rules! delete_all {
        ($entity:ident) => {
            if let Err(e) = $entity::Entity::delete_many().exec(db).await {
                return Err(format!("Failed to delete {}: {}", stringify!($entity), e));
            }
        };
    }

    // Delete in order of dependencies (child tables first)
    delete_all!(loan);
    delete_all!(copy);
    delete_all!(collection_book);
    delete_all!(collection);
    delete_all!(book_authors);
    delete_all!(book_tags);
    delete_all!(book);
    delete_all!(author);
    delete_all!(tag);

    delete_all!(p2p_outgoing_request);
    delete_all!(p2p_request);
    delete_all!(peer_book);
    delete_all!(peer);
    delete_all!(contact);

    delete_all!(operation_log);

    delete_all!(library_config);
    delete_all!(library);
    delete_all!(installation_profile);

    // Delete users too for complete reset
    delete_all!(user);

    Ok("App reset successfully - all data cleared".to_string())
}

// ============ HTTP Server (FFI) ============

/// Start the HTTP server on the specified port (FFI)
/// This is required for P2P functionality in standalone mode
/// If the specified port is occupied, tries the next 10 ports automatically
pub async fn start_server(port: u16) -> Result<u16, String> {
    let db = db().ok_or("Database not initialized")?.clone();

    // Try the specified port and fall back to alternatives if occupied
    let max_attempts = 10;
    let mut last_error = String::new();

    for offset in 0..max_attempts {
        let try_port = port.saturating_add(offset);
        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], try_port));

        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map_err(|e| format!("Failed to get local address: {}", e))?
                    .port();

                // Create a shared IdentityService and register it in the global
                // OnceLock so that init_identity_ffi() (called later by Flutter)
                // initializes the SAME instance. IdentityService uses Arc<OnceCell>
                // internally, so clones share the same identity state.
                let shared_id_svc = IDENTITY_SERVICE
                    .get_or_init(|| crate::services::IdentityService::new(db.clone()));
                let state = crate::infrastructure::AppState::with_identity_service(
                    db,
                    std::sync::Arc::new(shared_id_svc.clone()),
                );

                // Spawn relay poller (checks relay hub for incoming messages)
                let poller_state = state.clone();
                tokio::spawn(async move {
                    crate::services::relay_poller::start_relay_polling(
                        poller_state,
                        std::time::Duration::from_secs(60),
                    )
                    .await;
                });

                let api = crate::api::api_router_with_state(state);
                // Allow CORS for all origins/methods/headers for P2P ease
                let cors = CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any);

                let app = axum::Router::new()
                    .route(
                        "/invite",
                        axum::routing::get(crate::api::invite_page::invite_page),
                    )
                    .nest("/api", api)
                    .layer(cors);

                // Spawn server in background with panic catching
                let server_port = actual_port;
                tokio::spawn(async move {
                    tracing::info!("🚀 FFI Server task starting on port {}", server_port);
                    match axum::serve(listener, app).await {
                        Ok(()) => {
                            tracing::warn!(
                                "⚠️ FFI Server task exited normally on port {} (this is unexpected)",
                                server_port
                            );
                        }
                        Err(e) => {
                            tracing::error!("❌ FFI Server Error on port {}: {}", server_port, e);
                        }
                    }
                    tracing::error!(
                        "💀 FFI Server task ended on port {} - server is no longer running!",
                        server_port
                    );
                });

                if offset > 0 {
                    tracing::info!(
                        "✅ FFI: Port {} was occupied, server started on port {}",
                        port,
                        actual_port
                    );
                } else {
                    tracing::info!("✅ FFI: Server started on port {}", actual_port);
                }
                return Ok(actual_port);
            }
            Err(e) => {
                last_error = format!("{}", e);
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    tracing::debug!("Port {} occupied, trying {}", try_port, try_port + 1);
                    continue;
                } else {
                    // Non-recoverable error
                    return Err(format!("Failed to bind to port {}: {}", try_port, e));
                }
            }
        }
    }

    Err(format!(
        "Failed to bind to any port from {} to {}: {}",
        port,
        port + max_attempts - 1,
        last_error
    ))
}

// ============ Memory Game (FFI) ============

/// A card in the memory game (FFI-safe)
pub struct FrbMemoryCard {
    pub book_id: i32,
    pub title: String,
    pub cover_url: String,
}

/// A saved memory game score (FFI-safe)
pub struct FrbMemoryScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub pairs_count: i32,
    pub elapsed_seconds: f64,
    pub errors: i32,
    pub normalized_score: f64,
    pub played_at: String,
    /// Achievements unlocked after this game (empty if none)
    pub new_achievements: Vec<String>,
}

/// A leaderboard entry (FFI-safe)
pub struct FrbMemoryLeaderboardEntry {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
    /// True if this entry is the local user (not a peer)
    pub is_self: bool,
}

/// Get available difficulty levels based on books with covers
pub async fn memory_game_available_difficulties() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let difficulties = crate::modules::memory_game::service::available_difficulties(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(difficulties
        .iter()
        .map(|d| d.as_str().to_string())
        .collect())
}

/// Set up a new memory game with the given difficulty
pub async fn memory_game_setup(difficulty: String) -> Result<Vec<FrbMemoryCard>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let diff = crate::modules::memory_game::service::MemoryDifficulty::parse(&difficulty)
        .map_err(|e| e.to_string())?;
    let cards = crate::modules::memory_game::service::setup_game(&repo, diff)
        .await
        .map_err(|e| e.to_string())?;
    Ok(cards
        .into_iter()
        .map(|c| FrbMemoryCard {
            book_id: c.book_id,
            title: c.title,
            cover_url: c.cover_url,
        })
        .collect())
}

/// Submit a completed game and get the score back
pub async fn memory_game_finish(
    difficulty: String,
    elapsed_seconds: f64,
    errors: i32,
    pairs_count: i32,
) -> Result<FrbMemoryScore, String> {
    let db = db().ok_or("Database not initialized")?;
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let result = crate::modules::memory_game::domain::MemoryGameResult {
        difficulty,
        elapsed_seconds,
        errors,
        pairs_count,
    };
    let score = crate::modules::memory_game::service::finish_game(&game_repo, result)
        .await
        .map_err(|e| e.to_string())?;

    // Check achievements after game completion
    let new_achievements = {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        let puzzle_repo =
            crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
        crate::services::gamification_service::check_and_unlock_achievements(
            &gamification_repo,
            &game_repo,
            Some(&puzzle_repo),
        )
        .await
        .unwrap_or_default()
    };

    Ok(FrbMemoryScore {
        id: score.id,
        difficulty: score.difficulty,
        pairs_count: score.pairs_count,
        elapsed_seconds: score.elapsed_seconds,
        errors: score.errors,
        normalized_score: score.normalized_score,
        played_at: score.played_at,
        new_achievements,
    })
}

/// Get top memory game scores
pub async fn memory_game_top_scores() -> Result<Vec<FrbMemoryScore>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    use crate::modules::memory_game::domain::MemoryGameRepository;
    let scores = repo.get_top_scores(10).await.map_err(|e| e.to_string())?;
    Ok(scores
        .into_iter()
        .map(|s| FrbMemoryScore {
            id: s.id,
            difficulty: s.difficulty,
            pairs_count: s.pairs_count,
            elapsed_seconds: s.elapsed_seconds,
            errors: s.errors,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
            new_achievements: vec![],
        })
        .collect())
}

/// Get leaderboard (peer scores + local user's best)
pub async fn memory_game_leaderboard() -> Result<Vec<FrbMemoryLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    use crate::modules::memory_game::domain::MemoryGameRepository;

    // Peer scores
    let peer_scores = game_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbMemoryLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbMemoryLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score
    let top_scores = game_repo
        .get_top_scores(1)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(best) = top_scores.first() {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        entries.push(FrbMemoryLeaderboardEntry {
            peer_id: 0,
            library_name,
            best_score: best.normalized_score,
            difficulty: best.difficulty.clone(),
            played_at: best.played_at.clone(),
            is_self: true,
        });
    }

    // Sort by best_score descending
    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

/// Refresh the network memory game leaderboard by syncing with all accepted peers.
/// Fetches each peer's /api/game/memory/public-best, upserts into peer_memory_scores,
/// then returns the merged leaderboard.
pub async fn memory_game_refresh_leaderboard() -> Result<Vec<FrbMemoryLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    use crate::modules::memory_game::domain::MemoryGameRepository;

    // Check if memory_game module is enabled locally
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let local_enabled = match crate::models::installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
    {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"memory_game".to_string())
        }
        _ => true, // Default to enabled if no profile (dev mode)
    };

    if local_enabled {
        // Fetch all accepted peers
        let peers = crate::models::peer::Entity::find()
            .filter(crate::models::peer::Column::ConnectionStatus.eq("accepted"))
            .all(db)
            .await
            .unwrap_or_default();

        if !peers.is_empty() {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default();

            for peer in &peers {
                // Validate peer URL before making outbound request (SSRF protection)
                if crate::api::peer::validate_url(&peer.url).is_err() {
                    tracing::warn!("Skipping peer {} with invalid URL: {}", peer.id, peer.name);
                    continue;
                }

                // Fetch peer config to check enabled_modules
                let config_url = format!("{}/api/config", peer.url);
                let peer_has_memory_game = match client.get(&config_url).send().await {
                    Ok(res) if res.status().is_success() => {
                        match res.json::<crate::api::setup::ConfigResponse>().await {
                            Ok(config) => {
                                Some(config.enabled_modules.contains(&"memory_game".to_string()))
                            }
                            Err(_) => None,
                        }
                    }
                    _ => None,
                };

                crate::modules::memory_game::handlers::sync_peer_memory_scores(
                    db,
                    peer.id,
                    &peer.url,
                    &peer.name,
                    &client,
                    peer_has_memory_game,
                )
                .await;
            }
        }
    }

    // Return merged leaderboard (same logic as memory_game_leaderboard)
    let peer_scores = game_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbMemoryLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbMemoryLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score
    let top_scores = game_repo
        .get_top_scores(1)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(best) = top_scores.first() {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        entries.push(FrbMemoryLeaderboardEntry {
            peer_id: 0,
            library_name,
            best_score: best.normalized_score,
            difficulty: best.difficulty.clone(),
            played_at: best.played_at.clone(),
            is_self: true,
        });
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

// ============ Sliding Puzzle (FFI) ============

/// A generated puzzle board (FFI-safe)
pub struct FrbPuzzleBoard {
    pub book_id: i32,
    pub title: String,
    pub cover_url: String,
    pub grid_size: u8,
    pub tiles: Vec<u8>,
    pub empty_index: u32,
    pub par_moves: u32,
}

/// A saved sliding puzzle score (FFI-safe)
pub struct FrbPuzzleScore {
    pub id: Option<i32>,
    pub difficulty: String,
    pub grid_size: i32,
    pub elapsed_seconds: f64,
    pub move_count: i32,
    pub par_moves: i32,
    pub normalized_score: f64,
    pub played_at: String,
    /// Achievements unlocked after this game (empty if none)
    pub new_achievements: Vec<String>,
}

/// Get available puzzle difficulty levels based on books with covers
pub async fn puzzle_available_difficulties() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let difficulties = crate::modules::sliding_puzzle::service::available_difficulties(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(difficulties
        .iter()
        .map(|d| d.as_str().to_string())
        .collect())
}

/// Set up a new sliding puzzle with the given difficulty
pub async fn puzzle_setup(difficulty: String) -> Result<FrbPuzzleBoard, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let diff = crate::modules::sliding_puzzle::service::PuzzleDifficulty::parse(&difficulty)
        .map_err(|e| e.to_string())?;
    let board = crate::modules::sliding_puzzle::service::setup_game(&repo, diff)
        .await
        .map_err(|e| e.to_string())?;
    Ok(FrbPuzzleBoard {
        book_id: board.book_id,
        title: board.title,
        cover_url: board.cover_url,
        grid_size: board.grid_size,
        tiles: board.tiles,
        empty_index: board.empty_index as u32,
        par_moves: board.par_moves,
    })
}

/// Submit a completed sliding puzzle and get the score back
pub async fn puzzle_finish(
    difficulty: String,
    grid_size: u8,
    elapsed_seconds: f64,
    move_count: u32,
    par_moves: u32,
) -> Result<FrbPuzzleScore, String> {
    let db = db().ok_or("Database not initialized")?;
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    let result = crate::modules::sliding_puzzle::domain::PuzzleResult {
        difficulty,
        grid_size,
        elapsed_seconds,
        move_count,
        par_moves,
    };
    let score = crate::modules::sliding_puzzle::service::finish_game(&puzzle_repo, result)
        .await
        .map_err(|e| e.to_string())?;

    // Check achievements after game completion
    let new_achievements = {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        let game_repo =
            crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
        crate::services::gamification_service::check_and_unlock_achievements(
            &gamification_repo,
            &game_repo,
            Some(&puzzle_repo),
        )
        .await
        .unwrap_or_default()
    };

    Ok(FrbPuzzleScore {
        id: score.id,
        difficulty: score.difficulty,
        grid_size: score.grid_size,
        elapsed_seconds: score.elapsed_seconds,
        move_count: score.move_count,
        par_moves: score.par_moves,
        normalized_score: score.normalized_score,
        played_at: score.played_at,
        new_achievements,
    })
}

/// Get top sliding puzzle scores
pub async fn puzzle_top_scores() -> Result<Vec<FrbPuzzleScore>, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;
    let scores = repo.get_top_scores(10).await.map_err(|e| e.to_string())?;
    Ok(scores
        .into_iter()
        .map(|s| FrbPuzzleScore {
            id: s.id,
            difficulty: s.difficulty,
            grid_size: s.grid_size,
            elapsed_seconds: s.elapsed_seconds,
            move_count: s.move_count,
            par_moves: s.par_moves,
            normalized_score: s.normalized_score,
            played_at: s.played_at,
            new_achievements: vec![],
        })
        .collect())
}

/// A leaderboard entry for the sliding puzzle (FFI-safe)
pub struct FrbPuzzleLeaderboardEntry {
    pub peer_id: i32,
    pub library_name: String,
    pub best_score: f64,
    pub difficulty: String,
    pub played_at: String,
    pub is_self: bool,
}

/// Get puzzle leaderboard (peer scores + local user's best)
pub async fn puzzle_game_leaderboard() -> Result<Vec<FrbPuzzleLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;

    // Peer scores
    let peer_scores = puzzle_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbPuzzleLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbPuzzleLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score
    let top_scores = puzzle_repo
        .get_top_scores(1)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(best) = top_scores.first() {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        entries.push(FrbPuzzleLeaderboardEntry {
            peer_id: 0,
            library_name,
            best_score: best.normalized_score,
            difficulty: best.difficulty.clone(),
            played_at: best.played_at.clone(),
            is_self: true,
        });
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

/// Refresh the network puzzle leaderboard by syncing with all accepted peers.
/// Fetches each peer's /api/game/puzzle/public-best, upserts into peer_puzzle_scores,
/// then returns the merged leaderboard.
pub async fn puzzle_game_refresh_leaderboard() -> Result<Vec<FrbPuzzleLeaderboardEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    use crate::modules::sliding_puzzle::domain::SlidingPuzzleRepository;

    // Check if sliding_puzzle module is enabled locally
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let local_enabled = match crate::models::installation_profile::Entity::find_by_id(1)
        .one(db)
        .await
    {
        Ok(Some(p)) => {
            let modules: Vec<String> = serde_json::from_str(&p.enabled_modules).unwrap_or_default();
            modules.contains(&"sliding_puzzle".to_string())
        }
        _ => true, // Default to enabled if no profile (dev mode)
    };

    if local_enabled {
        // Fetch all accepted peers
        let peers = crate::models::peer::Entity::find()
            .filter(crate::models::peer::Column::ConnectionStatus.eq("accepted"))
            .all(db)
            .await
            .unwrap_or_default();

        if !peers.is_empty() {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default();

            for peer in &peers {
                // Validate peer URL before making outbound request (SSRF protection)
                if crate::api::peer::validate_url(&peer.url).is_err() {
                    tracing::warn!("Skipping peer {} with invalid URL: {}", peer.id, peer.name);
                    continue;
                }

                // Fetch peer config to check enabled_modules
                let config_url = format!("{}/api/config", peer.url);
                let peer_has_sliding_puzzle = match client.get(&config_url).send().await {
                    Ok(res) if res.status().is_success() => {
                        match res.json::<crate::api::setup::ConfigResponse>().await {
                            Ok(config) => Some(
                                config
                                    .enabled_modules
                                    .contains(&"sliding_puzzle".to_string()),
                            ),
                            Err(_) => None,
                        }
                    }
                    _ => None,
                };

                crate::modules::sliding_puzzle::handlers::sync_peer_puzzle_scores(
                    db,
                    peer.id,
                    &peer.url,
                    &peer.name,
                    &client,
                    peer_has_sliding_puzzle,
                )
                .await;
            }
        }
    }

    // Return merged leaderboard (same logic as puzzle_game_leaderboard)
    let peer_scores = puzzle_repo
        .get_peer_scores()
        .await
        .map_err(|e| e.to_string())?;
    let mut entries: Vec<FrbPuzzleLeaderboardEntry> = peer_scores
        .into_iter()
        .map(|s| FrbPuzzleLeaderboardEntry {
            peer_id: s.peer_id,
            library_name: s.library_name,
            best_score: s.best_score,
            difficulty: s.difficulty,
            played_at: s.played_at,
            is_self: false,
        })
        .collect();

    // Add local user's best score
    let top_scores = puzzle_repo
        .get_top_scores(1)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(best) = top_scores.first() {
        let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
        use crate::domain::GamificationRepository;
        let library_name = gamification_repo
            .get_library_name()
            .await
            .unwrap_or_else(|_| "My Library".to_string());

        entries.push(FrbPuzzleLeaderboardEntry {
            peer_id: 0,
            library_name,
            best_score: best.normalized_score,
            difficulty: best.difficulty.clone(),
            played_at: best.played_at.clone(),
            is_self: true,
        });
    }

    entries.sort_by(|a, b| {
        b.best_score
            .partial_cmp(&a.best_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(entries)
}

// ─── Gamification (FFI direct) ──────────────────────────────────────────────

/// Track progress (FFI-safe)
pub struct FrbTrackProgress {
    pub level: i32,
    pub progress: f32,
    pub current: i64,
    pub next_threshold: i32,
}

/// Streak info (FFI-safe)
pub struct FrbStreakInfo {
    pub current: i32,
    pub longest: i32,
}

/// Gamification config (FFI-safe)
pub struct FrbGamificationConfig {
    pub achievements_style: String,
    pub reading_goal_yearly: i32,
    pub reading_goal_progress: i32,
    pub total_books_read: i32,
}

/// Full gamification status (FFI-safe)
pub struct FrbGamificationStatus {
    pub collector: FrbTrackProgress,
    pub reader: FrbTrackProgress,
    pub lender: FrbTrackProgress,
    pub cataloguer: FrbTrackProgress,
    pub streak: FrbStreakInfo,
    pub recent_achievements: Vec<String>,
    pub config: FrbGamificationConfig,
    // Legacy fields
    pub level: String,
    pub loans_count: i64,
    pub edits_count: i64,
    pub next_level_progress: f32,
    pub badge_url: String,
}

/// Leaderboard entry (FFI-safe)
pub struct FrbLeaderboardEntry {
    pub library_name: String,
    pub level: i32,
    pub current: i64,
    pub is_self: bool,
    pub peer_id: Option<i32>,
}

/// Full leaderboard response (FFI-safe)
pub struct FrbLeaderboardResponse {
    pub collector: Vec<FrbLeaderboardEntry>,
    pub reader: Vec<FrbLeaderboardEntry>,
    pub lender: Vec<FrbLeaderboardEntry>,
    pub cataloguer: Vec<FrbLeaderboardEntry>,
    pub last_refreshed: Option<String>,
}

fn track_to_frb(t: &crate::services::gamification_service::TrackProgress) -> FrbTrackProgress {
    FrbTrackProgress {
        level: t.level,
        progress: t.progress,
        current: t.current,
        next_threshold: t.next_threshold,
    }
}

fn entries_to_frb(
    entries: &[crate::services::gamification_service::LeaderboardEntry],
) -> Vec<FrbLeaderboardEntry> {
    entries
        .iter()
        .map(|e| FrbLeaderboardEntry {
            library_name: e.library_name.clone(),
            level: e.level,
            current: e.current,
            is_self: e.is_self,
            peer_id: e.peer_id,
        })
        .collect()
}

/// Get full gamification status via FFI (replaces HTTP getUserStatus)
pub async fn gamification_get_status() -> Result<FrbGamificationStatus, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let status = crate::services::gamification_service::get_user_status(&repo)
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbGamificationStatus {
        collector: track_to_frb(&status.tracks.collector),
        reader: track_to_frb(&status.tracks.reader),
        lender: track_to_frb(&status.tracks.lender),
        cataloguer: track_to_frb(&status.tracks.cataloguer),
        streak: FrbStreakInfo {
            current: status.streak.current,
            longest: status.streak.longest,
        },
        recent_achievements: status.recent_achievements,
        config: FrbGamificationConfig {
            achievements_style: status.config.achievements_style,
            reading_goal_yearly: status.config.reading_goal_yearly,
            reading_goal_progress: status.config.reading_goal_progress,
            total_books_read: status.config.total_books_read,
        },
        level: status.level,
        loans_count: status.loans_count as i64,
        edits_count: status.edits_count as i64,
        next_level_progress: status.next_level_progress,
        badge_url: status.badge_url,
    })
}

/// Get leaderboard via FFI (replaces HTTP getLeaderboard)
pub async fn gamification_get_leaderboard() -> Result<FrbLeaderboardResponse, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let lb = crate::services::gamification_service::build_leaderboard(&repo)
        .await
        .map_err(|e| e.to_string())?;

    Ok(FrbLeaderboardResponse {
        collector: entries_to_frb(&lb.collector),
        reader: entries_to_frb(&lb.reader),
        lender: entries_to_frb(&lb.lender),
        cataloguer: entries_to_frb(&lb.cataloguer),
        last_refreshed: lb.last_refreshed,
    })
}

/// Refresh leaderboard (returns current state) via FFI.
/// Peer sync happens via the HTTP endpoint — this just returns current data.
pub async fn gamification_refresh_leaderboard() -> Result<FrbLeaderboardResponse, String> {
    gamification_get_leaderboard().await
}

/// Update gamification config via FFI
pub async fn gamification_update_config(
    reading_goal_yearly: Option<i32>,
    achievements_style: Option<String>,
) -> Result<(), String> {
    use crate::domain::GamificationRepository;
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let user_id = repo.get_user_id().await.map_err(|e| e.to_string())?;
    let update = crate::domain::GamificationConfigUpdate {
        reading_goal_yearly,
        achievements_style,
    };
    repo.update_config(user_id, update)
        .await
        .map_err(|e| e.to_string())
}

/// Check and unlock eligible achievements via FFI
pub async fn gamification_check_achievements() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    let gamification_repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let game_repo = crate::modules::memory_game::repository::SeaOrmGameRepository::new(db.clone());
    let puzzle_repo =
        crate::modules::sliding_puzzle::repository::SeaOrmPuzzleRepository::new(db.clone());
    crate::services::gamification_service::check_and_unlock_achievements(
        &gamification_repo,
        &game_repo,
        Some(&puzzle_repo),
    )
    .await
    .map_err(|e| e.to_string())
}

/// Update daily streak via FFI
pub async fn gamification_update_streak() -> Result<FrbStreakInfo, String> {
    let db = db().ok_or("Database not initialized")?;
    let repo = crate::infrastructure::repositories::gamification_repository::SeaOrmGamificationRepository::new(db.clone());
    let streak = crate::services::gamification_service::update_streak(&repo)
        .await
        .map_err(|e| e.to_string())?;
    Ok(FrbStreakInfo {
        current: streak.current,
        longest: streak.longest,
    })
}

// ── Operation Log Viewer FFI ──────────────────────────────────────────

#[frb(dart_metadata=("freezed"))]
pub struct FrbOperationLogEntry {
    pub id: i32,
    pub entity_type: String,
    pub entity_id: i32,
    pub operation: String,
    pub payload: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub pinned: bool,
    pub created_at: String,
}

#[frb(dart_metadata=("freezed"))]
pub struct FrbOperationLogStats {
    pub total: u64,
    pub today: u64,
    pub pending: u64,
    pub failed: u64,
}

/// List operation log entries with optional filters
pub async fn operation_log_list(
    entity_type: Option<String>,
    operation: Option<String>,
    status: Option<String>,
    query: Option<String>,
    page: Option<u64>,
    limit: Option<u64>,
) -> Result<Vec<FrbOperationLogEntry>, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::operation_log_viewer::domain::{
        OperationLogFilter, OperationLogViewerRepository,
    };
    use crate::modules::operation_log_viewer::repository::SeaOrmOperationLogViewerRepository;

    let repo = SeaOrmOperationLogViewerRepository::new(db);
    let filter = OperationLogFilter {
        entity_type,
        operation,
        status,
        query,
        since: None,
        until: None,
        page: page.unwrap_or(0),
        limit: limit.unwrap_or(50).min(200),
    };

    let page = repo.find_all(filter).await.map_err(|e| e.to_string())?;
    Ok(page
        .entries
        .into_iter()
        .map(|e| FrbOperationLogEntry {
            id: e.id,
            entity_type: e.entity_type,
            entity_id: e.entity_id,
            operation: e.operation,
            payload: e.payload,
            status: e.status,
            error_message: e.error_message,
            pinned: e.pinned,
            created_at: e.created_at,
        })
        .collect())
}

/// Get operation log stats
pub async fn operation_log_stats() -> Result<FrbOperationLogStats, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::operation_log_viewer::domain::OperationLogViewerRepository;
    use crate::modules::operation_log_viewer::repository::SeaOrmOperationLogViewerRepository;

    let repo = SeaOrmOperationLogViewerRepository::new(db);
    let stats = repo.get_stats().await.map_err(|e| e.to_string())?;
    Ok(FrbOperationLogStats {
        total: stats.total,
        today: stats.today,
        pending: stats.pending,
        failed: stats.failed,
    })
}

/// Get distinct entity types for filter dropdowns
pub async fn operation_log_entity_types() -> Result<Vec<String>, String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::modules::operation_log_viewer::domain::OperationLogViewerRepository;
    use crate::modules::operation_log_viewer::repository::SeaOrmOperationLogViewerRepository;

    let repo = SeaOrmOperationLogViewerRepository::new(db);
    repo.get_entity_types().await.map_err(|e| e.to_string())
}

// ============ Device Pairing (FFI) ============

use crate::services::device_pairing_service::DevicePairingService;

static DEVICE_PAIRING_SERVICE: OnceLock<std::sync::Arc<DevicePairingService>> = OnceLock::new();

/// Get or initialize the device pairing service
fn device_pairing_svc() -> Result<&'static std::sync::Arc<DevicePairingService>, String> {
    if let Some(svc) = DEVICE_PAIRING_SERVICE.get() {
        return Ok(svc);
    }
    let db_conn = db().ok_or("Database not initialized")?;
    let id_svc = IDENTITY_SERVICE.get().ok_or("Identity not initialized")?;
    let repo = std::sync::Arc::new(crate::infrastructure::SeaOrmLinkedDeviceRepository::new(
        db_conn.clone(),
    ));
    let id_arc = std::sync::Arc::new(id_svc.clone());
    let svc = std::sync::Arc::new(DevicePairingService::new(id_arc, repo));
    let _ = DEVICE_PAIRING_SERVICE.set(svc);
    Ok(DEVICE_PAIRING_SERVICE.get().unwrap())
}

/// FFI struct for linked device info
pub struct FrbLinkedDevice {
    pub id: i32,
    pub name: String,
    pub ed25519_public_key: Vec<u8>,
    pub x25519_public_key: Vec<u8>,
    pub relay_url: Option<String>,
    pub mailbox_id: Option<String>,
    pub last_synced: Option<String>,
    pub created_at: Option<String>,
}

/// FFI struct for pairing offer response
pub struct FrbPairingOffer {
    pub code: String,
    pub expires_in: u64,
}

/// FFI struct for pairing confirmation
pub struct FrbPairingConfirmation {
    pub device_id: i32,
    pub library_uuid: String,
    pub offerer_ed25519: Vec<u8>,
    pub offerer_x25519: Vec<u8>,
    pub offerer_relay_url: Option<String>,
    pub offerer_mailbox_id: Option<String>,
}

/// Generate a 6-digit pairing offer for multi-device linking
pub fn device_generate_pairing_offer(
    device_name: String,
    library_uuid: String,
    relay_url: Option<String>,
    mailbox_id: Option<String>,
    relay_write_token: Option<String>,
) -> Result<FrbPairingOffer, String> {
    let svc = device_pairing_svc()?;
    let resp = svc.generate_offer(
        device_name,
        library_uuid,
        relay_url,
        mailbox_id,
        relay_write_token,
    )?;
    Ok(FrbPairingOffer {
        code: resp.code,
        expires_in: resp.expires_in,
    })
}

/// Accept a pairing offer by entering the 6-digit code.
/// Returns the offerer's crypto keys and library info.
pub async fn device_accept_pairing(
    code: String,
    device_name: String,
    ed25519_public_key: Vec<u8>,
    x25519_public_key: Vec<u8>,
    relay_url: Option<String>,
    mailbox_id: Option<String>,
    relay_write_token: Option<String>,
) -> Result<FrbPairingConfirmation, String> {
    let svc = device_pairing_svc()?;
    let confirmation = svc
        .accept_offer(
            crate::services::device_pairing_service::PairingAcceptInput {
                code,
                device_name,
                ed25519_public_key,
                x25519_public_key,
                relay_url,
                mailbox_id,
                relay_write_token,
            },
        )
        .await?;
    Ok(FrbPairingConfirmation {
        device_id: confirmation.device_id,
        library_uuid: confirmation.library_uuid,
        offerer_ed25519: confirmation.offerer_ed25519,
        offerer_x25519: confirmation.offerer_x25519,
        offerer_relay_url: confirmation.offerer_relay_url,
        offerer_mailbox_id: confirmation.offerer_mailbox_id,
    })
}

/// List all linked devices
pub async fn device_list_linked() -> Result<Vec<FrbLinkedDevice>, String> {
    let svc = device_pairing_svc()?;
    let devices = svc.list_devices().await.map_err(|e| e.to_string())?;
    Ok(devices
        .into_iter()
        .map(|d| FrbLinkedDevice {
            id: d.id.unwrap_or(0),
            name: d.name,
            ed25519_public_key: d.ed25519_public_key,
            x25519_public_key: d.x25519_public_key,
            relay_url: d.relay_url,
            mailbox_id: d.mailbox_id,
            last_synced: d.last_synced,
            created_at: d.created_at,
        })
        .collect())
}

/// Remove a linked device by ID
pub async fn device_remove_linked(device_id: i32) -> Result<(), String> {
    let svc = device_pairing_svc()?;
    svc.remove_device(device_id)
        .await
        .map_err(|e| e.to_string())
}

// ============ Device Sync (FFI) ============

use crate::services::device_sync_service::DeviceSyncService;

static DEVICE_SYNC_SERVICE: OnceLock<std::sync::Arc<DeviceSyncService>> = OnceLock::new();

/// Get or initialize the device sync service
fn device_sync_svc() -> Result<&'static std::sync::Arc<DeviceSyncService>, String> {
    if let Some(svc) = DEVICE_SYNC_SERVICE.get() {
        return Ok(svc);
    }
    let db_conn = db().ok_or("Database not initialized")?;
    let repo = std::sync::Arc::new(crate::infrastructure::SeaOrmLinkedDeviceRepository::new(
        db_conn.clone(),
    ));
    let svc = std::sync::Arc::new(DeviceSyncService::new(db_conn.clone(), repo));
    let _ = DEVICE_SYNC_SERVICE.set(svc);
    Ok(DEVICE_SYNC_SERVICE.get().unwrap())
}

/// FFI struct for sync result
pub struct FrbSyncResult {
    pub sent_count: u32,
    pub received_count: u32,
    pub pending_review_count: u32,
}

/// FFI struct for pending review operation
pub struct FrbPendingReviewOp {
    pub id: i32,
    pub entity_type: String,
    pub entity_id: i32,
    pub operation: String,
    pub payload: Option<String>,
    pub source: String,
    pub created_at: String,
}

/// Trigger sync with a specific linked device.
/// This is a simplified version - full sync uses the HTTP trigger_sync endpoint
/// which handles E2EE transport. This FFI function delegates to it.
pub async fn device_trigger_sync(device_id: i32) -> Result<FrbSyncResult, String> {
    let svc = device_sync_svc()?;

    // Collect local ops to count what we would send
    let device = {
        let pairing_svc = device_pairing_svc()?;
        let devices = pairing_svc
            .list_devices()
            .await
            .map_err(|e| e.to_string())?;
        devices
            .into_iter()
            .find(|d| d.id == Some(device_id))
            .ok_or_else(|| "Device not found".to_string())?
    };

    let since = device.last_synced.as_deref();
    let local_ops = svc
        .get_local_ops_since(since)
        .await
        .map_err(|e| format!("Failed to get local ops: {e}"))?;

    let sent_count = local_ops.len() as u32;

    // Note: actual E2EE transport happens through the HTTP endpoint.
    // This FFI function returns the count of ops that would be sent.
    // The Flutter side should call the HTTP endpoint for actual sync.

    let pending_review_count = svc
        .get_pending_review_ops()
        .await
        .map(|ops| ops.len() as u32)
        .unwrap_or(0);

    Ok(FrbSyncResult {
        sent_count,
        received_count: 0, // Actual sync via HTTP
        pending_review_count,
    })
}

/// List operations pending review (sync safety mode)
pub async fn device_sync_pending_review() -> Result<Vec<FrbPendingReviewOp>, String> {
    let svc = device_sync_svc()?;
    let ops = svc
        .get_pending_review_ops()
        .await
        .map_err(|e| e.to_string())?;

    Ok(ops
        .into_iter()
        .map(|op| FrbPendingReviewOp {
            id: op.id,
            entity_type: op.entity_type,
            entity_id: op.entity_id,
            operation: op.operation,
            payload: op.payload,
            source: op.source,
            created_at: op.created_at,
        })
        .collect())
}

/// Approve specific pending review operations
pub async fn device_sync_approve(ids: Vec<i32>) -> Result<u32, String> {
    let svc = device_sync_svc()?;
    svc.approve_ops(&ids).await.map_err(|e| e.to_string())
}

/// Reject specific pending review operations
pub async fn device_sync_reject(ids: Vec<i32>) -> Result<u32, String> {
    let svc = device_sync_svc()?;
    svc.reject_ops(&ids).await.map_err(|e| e.to_string())
}

/// Approve all pending review operations at once
pub async fn device_sync_approve_all() -> Result<u32, String> {
    let svc = device_sync_svc()?;
    svc.approve_all_pending_review()
        .await
        .map_err(|e| e.to_string())
}

/// Reject all pending review operations at once
pub async fn device_sync_reject_all() -> Result<u32, String> {
    let svc = device_sync_svc()?;
    svc.reject_all_pending_review()
        .await
        .map_err(|e| e.to_string())
}
