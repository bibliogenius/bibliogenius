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
) -> Result<String, String> {
    tracing::info!(
        "🚀 mDNS FFI: init_mdns_ffi called with name='{}', port={}",
        library_name,
        port
    );

    match crate::services::mdns::init_mdns(&library_name, port, library_id) {
        Ok(_) => {
            tracing::info!("✅ mDNS FFI: Service started successfully");
            Ok("mDNS service started".to_string())
        }
        Err(e) => {
            tracing::error!("❌ mDNS FFI: Failed to start - {}", e);
            Err(e.to_string())
        }
    }
}

/// Stop mDNS service
pub async fn stop_mdns_ffi() -> Result<String, String> {
    crate::services::mdns::stop_mdns();
    Ok("mDNS service stopped".to_string())
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
        Ok(created_book) => Ok(FrbBook::from(created_book)),
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
        if let Some(subjects_json) = book.subjects {
            if let Ok(subjects) = serde_json::from_str::<Vec<String>>(&subjects_json) {
                for subject in subjects {
                    if !subject.trim().is_empty() {
                        *tag_counts.entry(subject.trim().to_string()).or_insert(0) += 1;
                    }
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
        Ok(t) => Ok(FrbTag {
            id: t.id,
            name: t.name,
            parent_id: t.parent_id,
            count: 0,
        }),
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
        Ok(t) => Ok(FrbTag {
            id: t.id,
            name: t.name,
            parent_id: t.parent_id,
            count: 0, // We don't query count on update
        }),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Delete a tag
pub async fn delete_tag(id: i32) -> Result<(), String> {
    let db = db().ok_or("Database not initialized")?;
    use crate::models::tag;
    use sea_orm::EntityTrait;

    match tag::Entity::delete_by_id(id).exec(db).await {
        Ok(_) => Ok(()),
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
        author, book, book_authors, book_tags, contact, copy, installation_profile, library,
        library_config, loan, operation_log, p2p_outgoing_request, p2p_request, peer, peer_book,
        tag, user,
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

                let api = crate::api::api_router(db);
                // Allow CORS for all origins/methods/headers for P2P ease
                let cors = CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any);

                let app = axum::Router::new().nest("/api", api).layer(cors);

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
