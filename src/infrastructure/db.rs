use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement};

use crate::utils::default_library_name::compute_default_library_name_seed;

/// Highest migration index applied by `run_migrations`.
///
/// Embedded in `.bgbackup` manifests (ADR-037 §2) so the restore pipeline
/// can decide whether to migrate the archived DB forward or refuse a
/// future-version archive. **Bump this constant whenever a new migration
/// is appended to `run_migrations`.**
pub const SCHEMA_VERSION: u32 = 79;

pub async fn init_db(database_url: &str) -> Result<DatabaseConnection, DbErr> {
    let db = Database::connect(database_url).await?;

    // Run migrations manually (simple SQL)
    run_migrations(&db).await?;

    Ok(db)
}

/// Run filesystem-side maintenance that does NOT require an open SeaORM
/// connection. Currently:
///
/// - Garbage-collect `*.rollback-<ts>` and `*.replaced-<ts>` siblings of the
///   live DB that are older than `ROLLBACK_TTL_SECONDS` (24h, ADR-037 §5).
///   Younger siblings are kept so the user can still hit "Restore previous
///   version" within the rollback window.
///
/// Best-effort: filesystem errors are logged and swallowed. Called from
/// `init_backend` after `init_db` succeeds.
pub fn run_startup_maintenance(db_path: &std::path::Path) {
    crate::api::backup::purge_expired_rollbacks(db_path);
}

pub async fn run_migrations(db: &DatabaseConnection) -> Result<(), DbErr> {
    // Create books table (new schema without author field)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS books (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            isbn TEXT,
            summary TEXT,
            publisher TEXT,
            publication_year INTEGER,
            dewey_decimal TEXT,
            lcc TEXT,
            subjects TEXT,
            marc_record TEXT,
            cataloguing_notes TEXT,
            source_data TEXT,
            shelf_position INTEGER,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Create library_config table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS library_config (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            description TEXT,
            tags TEXT NOT NULL DEFAULT '[]',
            latitude REAL,
            longitude REAL,
            share_location BOOLEAN DEFAULT 0,
            show_borrowed_books BOOLEAN DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 018: Add location fields to library_config
    // We attempt to add columns. If they exist, it might fail, so we ignore errors (simple migration strategy)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE library_config ADD COLUMN latitude REAL".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE library_config ADD COLUMN longitude REAL".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE library_config ADD COLUMN share_location INTEGER DEFAULT 0".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE library_config ADD COLUMN show_borrowed_books INTEGER DEFAULT 0"
                .to_owned(),
        ))
        .await;

    // Migration: Add user_rating to books table (0-10 scale, NULL = not rated)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN user_rating INTEGER DEFAULT NULL".to_owned(),
        ))
        .await;

    // Insert default library config if not exists. The seed name is computed
    // from the host machine so the standalone Rust binary (MCP/CLI/tests)
    // never registers itself as the literal placeholder "My Library". In FFI
    // mode Flutter overwrites this seed with its own device-aware default.
    let default_name = compute_default_library_name_seed();
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO library_config (id, name, description, tags, latitude, longitude, share_location, created_at, updated_at)
        VALUES (1, ?, 'Personal book collection', '[]', NULL, NULL, 0, datetime('now'), datetime('now'))
        "#,
        [default_name.into()],
    ))
    .await?;

    // Create installation_profile table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS installation_profile (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            profile_type TEXT NOT NULL DEFAULT 'individual',
            enabled_modules TEXT NOT NULL DEFAULT '[]',
            theme TEXT,
            avatar_config TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Insert default installation profile if not exists
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO installation_profile (id, profile_type, enabled_modules, theme, created_at, updated_at)
        VALUES (1, 'individual', '[]', 'default', datetime('now'), datetime('now'))
        "#
        .to_owned(),
    ))
    .await?;

    // Create users table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            totp_secret TEXT,
            role TEXT NOT NULL DEFAULT 'user',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Migration: Add totp_secret column if missing (for existing databases)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE users ADD COLUMN totp_secret TEXT".to_owned(),
        ))
        .await;

    // Create authors table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS authors (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Create tags table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Create book_authors junction table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS book_authors (
            book_id INTEGER NOT NULL,
            author_id INTEGER NOT NULL,
            PRIMARY KEY (book_id, author_id),
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE,
            FOREIGN KEY (author_id) REFERENCES authors(id) ON DELETE CASCADE
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Create book_tags junction table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS book_tags (
            book_id INTEGER NOT NULL,
            tag_id INTEGER NOT NULL,
            PRIMARY KEY (book_id, tag_id),
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE,
            FOREIGN KEY (tag_id) REFERENCES tags(id) ON DELETE CASCADE
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Create operation_log table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS operation_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT NOT NULL,
            entity_id INTEGER NOT NULL,
            operation TEXT NOT NULL, -- 'INSERT', 'UPDATE', 'DELETE'
            payload TEXT, -- JSON payload of the change
            created_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 004: Book/Copy Architecture Refactoring
    // Create libraries table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS libraries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            description TEXT,
            owner_id INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (owner_id) REFERENCES users(id)
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Default user (ID 1) removed for security.
    // Users must be created via the Setup/Registration flow.

    // Insert default library (ID 1) if it doesn't exist AND if user 1 exists
    // This is conditional to avoid FK constraint errors in test databases
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO libraries (id, name, description, owner_id, created_at, updated_at)
        SELECT 1, 'Default Library', 'Main library collection', 1, datetime('now'), datetime('now')
        WHERE EXISTS (SELECT 1 FROM users WHERE id = 1)
        "#
        .to_owned(),
    ))
    .await?;

    // Create copies table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS copies (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            book_id INTEGER NOT NULL,
            library_id INTEGER NOT NULL,
            acquisition_date TEXT,
            notes TEXT,
            status TEXT NOT NULL DEFAULT 'available',
            is_temporary INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE,
            FOREIGN KEY (library_id) REFERENCES libraries(id) ON DELETE CASCADE
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 005: Add status and is_temporary columns if they don't exist
    // Note: SQLite doesn't support IF NOT EXISTS in ALTER TABLE, so we ignore errors
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE copies ADD COLUMN status TEXT NOT NULL DEFAULT 'available'".to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE copies ADD COLUMN is_temporary INTEGER NOT NULL DEFAULT 0".to_owned(),
        ))
        .await;

    // Create indexes for copies
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_copies_status ON copies(status)".to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_copies_temporary ON copies(is_temporary)".to_owned(),
    ))
    .await?;

    // Create contacts table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS contacts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            name TEXT NOT NULL,
            first_name TEXT,
            email TEXT,
            phone TEXT,
            address TEXT,
            notes TEXT,
            user_id INTEGER,
            library_owner_id INTEGER NOT NULL,
            is_active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE SET NULL,
            FOREIGN KEY (library_owner_id) REFERENCES libraries(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_contacts_type ON contacts(type);
        CREATE INDEX IF NOT EXISTS idx_contacts_library_owner_id ON contacts(library_owner_id);
        CREATE INDEX IF NOT EXISTS idx_contacts_email ON contacts(email);
        CREATE INDEX IF NOT EXISTS idx_contacts_user_id ON contacts(user_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Migration: Add first_name column if missing (for existing databases)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN first_name TEXT".to_owned(),
        ))
        .await;

    // Create loans table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS loans (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            copy_id INTEGER NOT NULL,
            contact_id INTEGER NOT NULL,
            library_id INTEGER NOT NULL,
            loan_date TEXT NOT NULL,
            due_date TEXT NOT NULL,
            return_date TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            notes TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (copy_id) REFERENCES copies(id) ON DELETE CASCADE,
            FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE,
            FOREIGN KEY (library_id) REFERENCES libraries(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_loans_copy_id ON loans(copy_id);
        CREATE INDEX IF NOT EXISTS idx_loans_contact_id ON loans(contact_id);
        CREATE INDEX IF NOT EXISTS idx_loans_library_id ON loans(library_id);
        CREATE INDEX IF NOT EXISTS idx_loans_status ON loans(status);
        "#
        .to_owned(),
    ))
    .await?;

    // Create peers table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS peers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            public_key TEXT,
            latitude REAL,
            longitude REAL,
            last_seen TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_peers_url ON peers(url);
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 006: Peer Books Cache and Auto-Approve
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS peer_books (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            peer_id INTEGER NOT NULL,
            remote_book_id INTEGER NOT NULL,
            title TEXT NOT NULL,
            isbn TEXT,
            author TEXT,
            cover_url TEXT,
            summary TEXT,
            synced_at TEXT NOT NULL,
            FOREIGN KEY (peer_id) REFERENCES peers(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_peer_books_peer_id ON peer_books(peer_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Add auto_approve to peers
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN auto_approve INTEGER NOT NULL DEFAULT 0".to_owned(),
        ))
        .await;

    // Create p2p_requests table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS p2p_requests (
            id TEXT PRIMARY KEY,
            from_peer_id INTEGER NOT NULL,
            book_isbn TEXT NOT NULL,
            book_title TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (from_peer_id) REFERENCES peers(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_p2p_requests_from_peer_id ON p2p_requests(from_peer_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Create p2p_outgoing_requests table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS p2p_outgoing_requests (
            id TEXT PRIMARY KEY,
            to_peer_id INTEGER NOT NULL,
            book_isbn TEXT NOT NULL,
            book_title TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (to_peer_id) REFERENCES peers(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_p2p_outgoing_requests_to_peer_id ON p2p_outgoing_requests(to_peer_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 017: Add reading_status to books
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN reading_status TEXT NOT NULL DEFAULT 'to_read'"
                .to_owned(),
        ))
        .await;

    // Migration 019: Add avatar_config to installation_profile
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE installation_profile ADD COLUMN avatar_config TEXT".to_owned(),
        ))
        .await;

    // Migration 020: Add reading dates to books
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN started_reading_at TEXT".to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN finished_reading_at TEXT".to_owned(),
        ))
        .await;

    // Migration 021: Add cover_url to books table
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN cover_url TEXT".to_owned(),
        ))
        .await;

    // ============================================
    // Gamification V3 Migrations (Migration 021)
    // ============================================

    // Gamification Config - Feature flags per user
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS gamification_config (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL UNIQUE,
            preset TEXT NOT NULL DEFAULT 'individual',
            streaks_enabled INTEGER NOT NULL DEFAULT 1,
            achievements_enabled INTEGER NOT NULL DEFAULT 1,
            achievements_style TEXT NOT NULL DEFAULT 'minimal',
            reading_goals_enabled INTEGER NOT NULL DEFAULT 1,
            reading_goal_yearly INTEGER NOT NULL DEFAULT 12,
            tracks_enabled TEXT NOT NULL DEFAULT '["collector","reader","lender"]',
            notifications_enabled INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Gamification Progress - Track progress per user
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS gamification_progress (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            track TEXT NOT NULL,
            current_value INTEGER NOT NULL DEFAULT 0,
            level INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(user_id, track),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_gamification_progress_user ON gamification_progress(user_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Gamification Achievements - Unlocked achievements per user
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS gamification_achievements (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            achievement_id TEXT NOT NULL,
            unlocked_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(user_id, achievement_id),
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_gamification_achievements_user ON gamification_achievements(user_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Gamification Streaks - Activity streaks per user
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS gamification_streaks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL UNIQUE,
            current_streak INTEGER NOT NULL DEFAULT 0,
            longest_streak INTEGER NOT NULL DEFAULT 0,
            last_activity_date TEXT,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Initialize gamification data for existing users (if any)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO gamification_config (user_id, preset, created_at, updated_at)
        SELECT id, 'individual', datetime('now'), datetime('now') FROM users
        "#
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO gamification_progress (user_id, track, created_at, updated_at)
        SELECT id, 'collector', datetime('now'), datetime('now') FROM users
        "#
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO gamification_progress (user_id, track, created_at, updated_at)
        SELECT id, 'reader', datetime('now'), datetime('now') FROM users
        "#
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO gamification_progress (user_id, track, created_at, updated_at)
        SELECT id, 'lender', datetime('now'), datetime('now') FROM users
        "#
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO gamification_streaks (user_id)
        SELECT id FROM users
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 022: Add structured address fields to contacts
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN street_address TEXT".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN postal_code TEXT".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN city TEXT".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN country TEXT".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN latitude REAL".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE contacts ADD COLUMN longitude REAL".to_owned(),
        ))
        .await;

    // Migration 023: Add owned field to books (controls automatic copy creation)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN owned INTEGER NOT NULL DEFAULT 1".to_owned(),
        ))
        .await;

    // Set owned=0 for books with reading_status='wanting' (wishlist items)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "UPDATE books SET owned = 0 WHERE reading_status = 'wanting'".to_owned(),
        ))
        .await;

    // Backfill: Create copies for books that have owned=1 but no copy
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            INSERT INTO copies (book_id, library_id, status, is_temporary, created_at, updated_at)
            SELECT b.id, 1, 'available', 0, datetime('now'), datetime('now')
            FROM books b
            LEFT JOIN copies c ON c.book_id = b.id
            WHERE c.id IS NULL AND b.owned = 1
            "#
            .to_owned(),
        ))
        .await;

    // Migration 024: Add status and error_message to operation_log
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE operation_log ADD COLUMN status TEXT NOT NULL DEFAULT 'pending'"
                .to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE operation_log ADD COLUMN error_message TEXT".to_owned(),
        ))
        .await;

    // Migration 025: Add library_uuid to peers for P2P deduplication
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN library_uuid TEXT".to_owned(),
        ))
        .await;

    // Create index for library_uuid lookups
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_peers_library_uuid ON peers(library_uuid)".to_owned(),
        ))
        .await;

    // Migration 026: Add hierarchical tags support (parent_id for tree structure, path for fast lookups)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE tags ADD COLUMN parent_id INTEGER REFERENCES tags(id) ON DELETE SET NULL"
                .to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE tags ADD COLUMN path TEXT DEFAULT ''".to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_tags_parent ON tags(parent_id)".to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_tags_path ON tags(path)".to_owned(),
        ))
        .await;

    // Migration 027: Add price to books
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN price REAL".to_owned(),
        ))
        .await;

    // Migration 028: Add digital_formats to books
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN digital_formats TEXT".to_owned(),
        ))
        .await;

    // Migration 028: Add price to copies
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE copies ADD COLUMN price REAL".to_owned(),
        ))
        .await;

    // Migration 029: Create sales table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS sales (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            copy_id INTEGER NOT NULL,
            contact_id INTEGER,
            library_id INTEGER NOT NULL,
            sale_date TEXT NOT NULL,
            sale_price REAL NOT NULL,
            status TEXT NOT NULL DEFAULT 'completed',
            notes TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            FOREIGN KEY (copy_id) REFERENCES copies(id) ON DELETE CASCADE,
            FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE SET NULL,
            FOREIGN KEY (library_id) REFERENCES libraries(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_sales_copy_id ON sales(copy_id);
        CREATE INDEX IF NOT EXISTS idx_sales_contact_id ON sales(contact_id);
        CREATE INDEX IF NOT EXISTS idx_sales_library_id ON sales(library_id);
        CREATE INDEX IF NOT EXISTS idx_sales_status ON sales(status);
        CREATE INDEX IF NOT EXISTS idx_sales_date ON sales(sale_date);
        CREATE INDEX IF NOT EXISTS idx_sales_created_at ON sales(created_at);
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 030: Add sold_at to copies
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE copies ADD COLUMN sold_at TEXT".to_owned(),
        ))
        .await;

    // Migration 031: Create collections and collection_books tables
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS collections (
            id TEXT PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            description TEXT,
            source TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS collection_books (
            collection_id TEXT NOT NULL,
            book_id INTEGER NOT NULL,
            added_at TEXT NOT NULL,
            PRIMARY KEY (collection_id, book_id),
            FOREIGN KEY (collection_id) REFERENCES collections(id) ON DELETE CASCADE,
            FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_collection_books_collection ON collection_books(collection_id);
        CREATE INDEX IF NOT EXISTS idx_collection_books_book ON collection_books(book_id);
        "#
        .to_owned(),
    ))
    .await?;

    // Migration 032: Convert 'owned' reading_status to empty string (harmonization)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "UPDATE books SET reading_status = '' WHERE reading_status = 'owned'".to_owned(),
        ))
        .await;

    // Migration 033: Add connection_status to peers (decoupled from auto_approve)
    // Default 'accepted' so existing peers remain connected
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN connection_status TEXT NOT NULL DEFAULT 'accepted'"
                .to_owned(),
        ))
        .await;

    // Migration 034: Create peer_gamification_stats table for network leaderboard
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE IF NOT EXISTS peer_gamification_stats (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                peer_id INTEGER NOT NULL,
                library_name TEXT NOT NULL,
                collector_level INTEGER NOT NULL DEFAULT 0,
                collector_current INTEGER NOT NULL DEFAULT 0,
                reader_level INTEGER NOT NULL DEFAULT 0,
                reader_current INTEGER NOT NULL DEFAULT 0,
                lender_level INTEGER NOT NULL DEFAULT 0,
                lender_current INTEGER NOT NULL DEFAULT 0,
                cataloguer_level INTEGER NOT NULL DEFAULT 0,
                cataloguer_current INTEGER NOT NULL DEFAULT 0,
                synced_at TEXT NOT NULL,
                FOREIGN KEY (peer_id) REFERENCES peers(id) ON DELETE CASCADE
            )"
            .to_owned(),
        ))
        .await;

    // Migration 035: Add api_keys to installation_profile (JSON, e.g. {"google_books": "AIza..."})
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE installation_profile ADD COLUMN api_keys TEXT".to_owned(),
        ))
        .await;

    // Migration 036: E2EE — crypto_keys table (stores encrypted identity keypairs)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS crypto_keys (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            key_type TEXT NOT NULL,
            public_key BLOB NOT NULL,
            encrypted_secret BLOB NOT NULL,
            salt BLOB NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            revoked_at TEXT,
            FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
        )"#
        .to_owned(),
    ))
    .await?;

    // Migration 037: E2EE — seen_envelopes table for replay protection (B4)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS seen_envelopes (
            nonce BLOB PRIMARY KEY,
            received_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"#
        .to_owned(),
    ))
    .await?;

    // Index for periodic cleanup of old seen_envelopes
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_seen_envelopes_received_at ON seen_envelopes(received_at)"
                .to_owned(),
        ))
        .await;

    // Migration 038: E2EE Phase 2 — peers.x25519_public_key (hex-encoded X25519 public key)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN x25519_public_key TEXT".to_owned(),
        ))
        .await;

    // Migration 039: E2EE Phase 2 — peers.key_exchange_done (both keys exchanged successfully)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN key_exchange_done INTEGER NOT NULL DEFAULT 0".to_owned(),
        ))
        .await;

    // Migration 040: E2EE Phase 2 — peers.mailbox_id (for offline message relay)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN mailbox_id TEXT".to_owned(),
        ))
        .await;

    // Migration 041: E2EE — Remove FK constraint on crypto_keys.user_id
    // Node identity is per-device, not per-user. The FK caused failures when
    // identity init runs before user creation (setup flow).
    // SQLite doesn't support DROP CONSTRAINT, so we recreate the table.
    let has_fk: bool = match db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='crypto_keys'".to_owned(),
        ))
        .await
    {
        Ok(Some(row)) => {
            let sql: String = row.try_get("", "sql").unwrap_or_default();
            sql.contains("FOREIGN KEY")
        }
        _ => false,
    };

    if has_fk {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                r#"
                CREATE TABLE IF NOT EXISTS crypto_keys_new (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id INTEGER NOT NULL DEFAULT 0,
                    key_type TEXT NOT NULL,
                    public_key BLOB NOT NULL,
                    encrypted_secret BLOB NOT NULL,
                    salt BLOB NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    revoked_at TEXT
                )"#
                .to_owned(),
            ))
            .await;
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO crypto_keys_new SELECT * FROM crypto_keys".to_owned(),
            ))
            .await;
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "DROP TABLE crypto_keys".to_owned(),
            ))
            .await;
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "ALTER TABLE crypto_keys_new RENAME TO crypto_keys".to_owned(),
            ))
            .await;
    }

    // Migration 042: Add lender_request_id to p2p_outgoing_requests
    // Stores the lender's p2p_request.id so the borrower can notify
    // the lender when returning a book.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE p2p_outgoing_requests ADD COLUMN lender_request_id TEXT".to_owned(),
        ))
        .await;

    // Migration 043: E2EE Phase 4 — Relay WAN tables and peer relay columns

    // Relay mailboxes (one per user on this relay hub)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS relay_mailboxes (
            uuid TEXT PRIMARY KEY,
            read_token TEXT NOT NULL,
            write_token TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            last_accessed TEXT
        )"#
        .to_owned(),
    ))
    .await?;

    // Relay messages (opaque encrypted blobs stored on the hub)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS relay_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            mailbox_uuid TEXT NOT NULL,
            blob BLOB NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            FOREIGN KEY (mailbox_uuid) REFERENCES relay_mailboxes(uuid) ON DELETE CASCADE
        )"#
        .to_owned(),
    ))
    .await?;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_relay_messages_mailbox ON relay_messages(mailbox_uuid)"
                .to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_relay_messages_created ON relay_messages(created_at)"
                .to_owned(),
        ))
        .await;

    // Peer relay columns (for reaching peers via relay)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN relay_url TEXT".to_owned(),
        ))
        .await;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN relay_write_token TEXT".to_owned(),
        ))
        .await;

    // Local relay config (singleton: my mailbox on the relay hub)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS my_relay_config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            relay_url TEXT NOT NULL,
            mailbox_uuid TEXT NOT NULL,
            read_token TEXT NOT NULL,
            write_token TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"#
        .to_owned(),
    ))
    .await?;

    // Migration 044: Add requester_request_id to p2p_requests (loan ID from borrower's side)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE p2p_requests ADD COLUMN requester_request_id TEXT".to_owned(),
        ))
        .await;

    // Migration 047: Multi-device sync - linked devices registry
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"CREATE TABLE IF NOT EXISTS linked_devices (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            name                TEXT NOT NULL,
            ed25519_public_key  BLOB NOT NULL,
            x25519_public_key   BLOB NOT NULL,
            relay_url           TEXT,
            mailbox_id          TEXT,
            relay_write_token   TEXT,
            last_synced         TEXT,
            created_at          TEXT NOT NULL DEFAULT (datetime('now'))
        )"#
        .to_owned(),
    ))
    .await?;

    // Migration 048: Add pinned column to operation_log for milestone preservation
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE operation_log ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0".to_owned(),
        ))
        .await;

    // Migration 049: Add source column to operation_log for multi-device sync echo prevention
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE operation_log ADD COLUMN source TEXT NOT NULL DEFAULT 'local'".to_owned(),
        ))
        .await;

    // Migration 050: Add catalog tracking columns to peers for relay library sync (ADR-012)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN catalog_hash TEXT".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN last_catalog_sync TEXT".to_owned(),
        ))
        .await;

    // Migration 051: Hub directory config — stores local settings for the public directory feature (ADR-015)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE IF NOT EXISTS hub_directory_config (
                id               INTEGER PRIMARY KEY DEFAULT 1,
                node_id          TEXT NOT NULL,
                write_token      TEXT NOT NULL,
                is_listed        INTEGER NOT NULL DEFAULT 0,
                requires_approval INTEGER NOT NULL DEFAULT 1,
                accept_from      TEXT NOT NULL DEFAULT 'everyone',
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL
            )"
            .to_owned(),
        ))
        .await;

    // Migration 052: Remove legacy library-type contacts.
    // The "library" contact type is superseded by hub follows (directory) and
    // direct peers (P2P). Loans referencing these contacts are also removed -
    // SQLite does not enforce FK constraints by default, so we clean up explicitly.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM loans WHERE contact_id IN (SELECT id FROM contacts WHERE type = 'library')"
                .to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM contacts WHERE type = 'library'".to_owned(),
        ))
        .await;

    // Migration 053: Add display_name to peers table for user-defined labels.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN display_name TEXT".to_owned(),
        ))
        .await;

    // Migration 054: Library view stats - tracks peer and follower catalog views per day.
    // Bounded to 365 days x 2 sources = max 730 rows.
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"CREATE TABLE IF NOT EXISTS library_view_stats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            date TEXT NOT NULL,
            source TEXT NOT NULL,
            count INTEGER NOT NULL DEFAULT 0,
            UNIQUE(date, source)
        )"#
        .to_owned(),
    ))
    .await?;

    // Prune old view stats beyond 365 days
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "DELETE FROM library_view_stats WHERE date < date('now', '-365 days')".to_owned(),
        ))
        .await;

    // Migration 055: Add allow_borrowing to hub_directory_config (defaults to enabled).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE hub_directory_config ADD COLUMN allow_borrowing INTEGER NOT NULL DEFAULT 1"
                .to_owned(),
        ))
        .await;

    // Migration 056: Add node_id and first_seen_at to peer_books for dedup
    // and "new" badge support. node_id links peer books to hub directory entries
    // so the same library is not cached twice. first_seen_at tracks when a book
    // was first discovered (survives upsert syncs).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peer_books ADD COLUMN node_id TEXT".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peer_books ADD COLUMN first_seen_at TEXT".to_owned(),
        ))
        .await;
    // Backfill: existing entries get first_seen_at = synced_at (best approximation)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "UPDATE peer_books SET first_seen_at = synced_at WHERE first_seen_at IS NULL"
                .to_owned(),
        ))
        .await;

    // Migration 057: Deduplicate books with the same ISBN.
    // Keeps the oldest entry (lowest id) per ISBN. Reassigns all FK references
    // (copies, collection_books, book_authors, book_tags) to the kept entry,
    // then deletes the duplicate rows.
    //
    // Books without an ISBN (NULL or empty string) are NEVER considered
    // duplicates of each other — ISBN is optional (self-published, rare,
    // ancient, personal publications) and grouping by empty ISBN would
    // silently delete legitimately distinct books. Every query below must
    // filter `isbn IS NOT NULL AND isbn <> ''`.
    //
    // First, normalise any legacy rows where `isbn = ''` to NULL so the
    // SeaORM layer and all subsequent queries see a single "no ISBN" value.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "UPDATE books SET isbn = NULL WHERE isbn IS NOT NULL AND trim(isbn) = ''".to_owned(),
        ))
        .await;

    // Reassign copies to the kept book
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            UPDATE copies SET book_id = (
                SELECT MIN(b2.id) FROM books b2
                WHERE b2.isbn = (SELECT isbn FROM books WHERE id = copies.book_id)
                  AND b2.isbn IS NOT NULL AND b2.isbn <> ''
            )
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL AND isbn <> ''
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books
                    WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
                )
            )
            "#
            .to_owned(),
        ))
        .await;

    // Reassign collection_books to the kept book
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            UPDATE collection_books SET book_id = (
                SELECT MIN(b2.id) FROM books b2
                WHERE b2.isbn = (SELECT isbn FROM books WHERE id = collection_books.book_id)
                  AND b2.isbn IS NOT NULL AND b2.isbn <> ''
            )
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL AND isbn <> ''
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books
                    WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
                )
            )
            "#
            .to_owned(),
        ))
        .await;

    // Reassign book_authors to the kept book (ignore conflicts - junction table PK)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            INSERT OR IGNORE INTO book_authors (book_id, author_id)
            SELECT (SELECT MIN(b2.id) FROM books b2
                    WHERE b2.isbn = (SELECT isbn FROM books WHERE id = book_authors.book_id)
                      AND b2.isbn IS NOT NULL AND b2.isbn <> ''),
                   author_id
            FROM book_authors
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL AND isbn <> ''
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books
                    WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
                )
            )
            "#
            .to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            DELETE FROM book_authors WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL AND isbn <> ''
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books
                    WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
                )
            )
            "#
            .to_owned(),
        ))
        .await;

    // Reassign book_tags to the kept book (ignore conflicts - junction table PK)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            INSERT OR IGNORE INTO book_tags (book_id, tag_id)
            SELECT (SELECT MIN(b2.id) FROM books b2
                    WHERE b2.isbn = (SELECT isbn FROM books WHERE id = book_tags.book_id)
                      AND b2.isbn IS NOT NULL AND b2.isbn <> ''),
                   tag_id
            FROM book_tags
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL AND isbn <> ''
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books
                    WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
                )
            )
            "#
            .to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            DELETE FROM book_tags WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL AND isbn <> ''
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books
                    WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
                )
            )
            "#
            .to_owned(),
        ))
        .await;

    // Remove duplicate collection_books links (same collection + book_id after reassign)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            DELETE FROM collection_books WHERE rowid NOT IN (
                SELECT MIN(rowid) FROM collection_books GROUP BY collection_id, book_id
            )
            "#
            .to_owned(),
        ))
        .await;

    // Delete the duplicate book rows (keep oldest per ISBN). The
    // `isbn <> ''` guard is load-bearing: without it, SQLite treats every
    // ISBN-less row as belonging to the same group and silently deletes all
    // but one on every app start.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            DELETE FROM books WHERE isbn IS NOT NULL AND isbn <> '' AND id NOT IN (
                SELECT MIN(id) FROM books
                WHERE isbn IS NOT NULL AND isbn <> '' GROUP BY isbn
            )
            "#
            .to_owned(),
        ))
        .await;

    // Migration 058: Activity feed notifications table (ADR-020)
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS notifications (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            category TEXT NOT NULL,
            title TEXT NOT NULL,
            body TEXT,
            ref_type TEXT,
            ref_id TEXT,
            read_at TEXT,
            created_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_notifications_category ON notifications(category)"
                .to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_notifications_created ON notifications(created_at)"
                .to_owned(),
        ))
        .await;

    // Migration 059: Add notified_at to peer_books for notification dedup.
    // Tracks whether the user has already been notified about a book from a peer.
    // Prevents re-emission of "new_books" and "wishlist_match" notifications
    // after the original notification is pruned by TTL/cap.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peer_books ADD COLUMN notified_at TEXT".to_owned(),
        ))
        .await;
    // Backfill: mark all existing entries as already notified
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "UPDATE peer_books SET notified_at = synced_at WHERE notified_at IS NULL".to_owned(),
        ))
        .await;

    // Migration 060: Add private flag to books.
    // When true, the book is hidden from peers (not shared on the network).
    // Default false: all existing books remain visible (no regression).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN private INTEGER NOT NULL DEFAULT 0".to_owned(),
        ))
        .await;

    // Migration 061: Add page_count to books.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN page_count INTEGER".to_owned(),
        ))
        .await;

    // Migration 062: Loan settings (customizable loan duration).
    // - New table `loan_settings` with global default duration + per-book toggle.
    // - New column `loan_duration_days` on `books` for per-book override.
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS loan_settings (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            default_loan_duration_days INTEGER NOT NULL DEFAULT 21,
            per_book_duration_enabled INTEGER NOT NULL DEFAULT 0
        )
        "#
        .to_owned(),
    ))
    .await?;

    // Seed a single row if the table is empty
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO loan_settings (id, default_loan_duration_days, per_book_duration_enabled)
        VALUES (1, 21, 0)
        "#
        .to_owned(),
    ))
    .await?;

    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN loan_duration_days INTEGER".to_owned(),
        ))
        .await;

    // Migration 063: Add avatar_config to peers (JSON from remote peer's profile).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN avatar_config TEXT".to_owned(),
        ))
        .await;

    // Migration 064: Add recovery_code to hub_directory_config (post-reinstall recovery).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE hub_directory_config ADD COLUMN recovery_code TEXT".to_owned(),
        ))
        .await;

    // Migration 066: Add reminder_days_before_due to loan_settings.
    // Controls how many days before the due date the first loan reminder is sent.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE loan_settings ADD COLUMN reminder_days_before_due INTEGER NOT NULL DEFAULT 2".to_owned(),
        ))
        .await;

    // Migration 065: Deactivate stale Library contacts.
    // Library-type contacts are auto-created when a peer lends/borrows a book.
    // They were not cleaned up when their associated peer was deleted, causing them
    // to appear as selectable contacts in the borrow dialog. This migration deactivates
    // any Library contact whose peer no longer exists (matched by name).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"UPDATE contacts
               SET is_active = 0,
                   updated_at = datetime('now')
               WHERE type = 'Library'
                 AND is_active = 1
                 AND name NOT IN (SELECT name FROM peers)"#
                .to_owned(),
        ))
        .await;

    // Migration 067: Reset peer_books.first_seen_at populated by migration 056.
    // 056 backfilled `first_seen_at = synced_at` as a "best approximation",
    // tagging every pre-existing peer book as "new" for 7 days. Backfilled rows
    // and fresh inserts cannot be distinguished after subsequent syncs, so this
    // migration NULLs every row — equivalent to a "first display" reset. Only
    // books inserted AFTER this migration runs will get a first_seen_at and
    // therefore the "new" badge.
    //
    // Idempotency via _migration_log so the reset doesn't reapply on every
    // app start (which would wipe legitimate first_seen_at on new inserts).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE TABLE IF NOT EXISTS _migration_log (\
                name TEXT PRIMARY KEY,\
                applied_at TEXT NOT NULL\
             )"
            .to_owned(),
        ))
        .await;
    let already_applied = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM _migration_log WHERE name = '067_reset_peer_books_first_seen_at'"
                .to_owned(),
        ))
        .await
        .ok()
        .flatten()
        .is_some();
    if !already_applied {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "UPDATE peer_books SET first_seen_at = NULL".to_owned(),
            ))
            .await;
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO _migration_log (name, applied_at) \
                 VALUES ('067_reset_peer_books_first_seen_at', datetime('now'))"
                    .to_owned(),
            ))
            .await;
    }

    // Migration 068: Add last_catalog_hash to hub_directory_config.
    // Stores the SHA-256 of the last successful catalog push so the client
    // can short-circuit identical re-pushes without a network round-trip
    // (ADR-027).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE hub_directory_config ADD COLUMN last_catalog_hash TEXT".to_owned(),
        ))
        .await;

    // Migration 069: Composite index on operation_log(entity_type, id).
    // Backs the delta sync endpoint (ADR-028): every peer pull issues a
    // `WHERE entity_type = ? AND id > ? ORDER BY id` query that would
    // otherwise scan the full log as it grows past the 90-day / 10k-row
    // retention floor.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_operation_log_entity_id ON operation_log(entity_type, id)".to_owned(),
        ))
        .await;

    // Migration 070: Per-peer delta sync cursor (ADR-028). Stores the last
    // `operation_log.id` we successfully applied from this peer, used as
    // `?since=<cursor>` on the next pull. NULL means "no successful sync
    // yet" — the next pull will be a full GET.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN last_delta_cursor INTEGER".to_owned(),
        ))
        .await;

    // Migration 071: Add `added_at` to peer_books. Replaces the per-device
    // `first_seen_at` for the "new" badge: the owner's `books.created_at`
    // is now broadcast to peers as `Book.added_at`, so every viewer agrees
    // on whether a book is recent. `first_seen_at` stays in the schema for
    // legacy hub-directory cache rows (peer_id = 0) which don't yet carry
    // an `added_at`.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peer_books ADD COLUMN added_at TEXT".to_owned(),
        ))
        .await;

    // Migration 072: Track failed hub cover uploads so the owner's UI can
    // surface a warning badge while retries pend. NULL = no pending failure
    // (either never attempted, or last attempt succeeded). Reset to NULL on
    // successful upload and on hub purge (library unregistered).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books ADD COLUMN hub_cover_upload_failed_at TEXT".to_owned(),
        ))
        .await;

    // Migration 073: Cache peer loan status in `peer_books`. Without these,
    // the carousel can't tell which of a peer's books are actually
    // requestable. Legacy rows default to owned=true / available_copies=NULL
    // (treated as "unknown, keep"); the companion cursor reset below forces
    // every peer to resync from scratch so the new columns actually get
    // populated — without it a stale book unchanged since pre-073 would
    // never get refreshed by the delta flow.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peer_books ADD COLUMN owned INTEGER NOT NULL DEFAULT 1".to_owned(),
        ))
        .await;
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peer_books ADD COLUMN available_copies INTEGER".to_owned(),
        ))
        .await;
    // One-time full-resync trigger (idempotency via `_migration_log`, same
    // pattern as migration 067). Every run would otherwise blow the cursor
    // away and force a full catalog pull on every boot.
    let already_applied = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM _migration_log WHERE name = '073_reset_peer_delta_cursor'".to_owned(),
        ))
        .await
        .ok()
        .flatten()
        .is_some();
    if !already_applied {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "UPDATE peers SET last_delta_cursor = NULL".to_owned(),
            ))
            .await;
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO _migration_log (name, applied_at) \
                 VALUES ('073_reset_peer_delta_cursor', datetime('now'))"
                    .to_owned(),
            ))
            .await;
    }

    // Migration 074: Mark a peer's `relay_write_token` as dead after a 404
    // that couldn't be recovered by credential refresh (ADR-032). Stops the
    // deposit-retry flood against a mailbox the hub no longer has. NULL =
    // valid, ISO 8601 timestamp = invalidated at that time. Cleared when a
    // fresh write_token is persisted (handshake, refresh success).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE peers ADD COLUMN relay_write_token_invalid_at TEXT".to_owned(),
        ))
        .await;

    // Migration 075: Typed loan metadata on `copies` (ADR-034). Replaces the
    // free-text `notes` schema previously used to carry the lender name and
    // due date. `notes` is preserved and still double-written for one
    // release cycle so non-upgraded clients keep rendering borrowed copies.
    for stmt in [
        "ALTER TABLE copies ADD COLUMN lender_display_name TEXT",
        "ALTER TABLE copies ADD COLUMN lender_peer_id INTEGER REFERENCES peers(id) ON DELETE SET NULL",
        "ALTER TABLE copies ADD COLUMN borrow_due_date TEXT",
        "ALTER TABLE copies ADD COLUMN borrow_source TEXT",
    ] {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                stmt.to_owned(),
            ))
            .await;
    }
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "CREATE INDEX IF NOT EXISTS idx_copies_borrow_source ON copies(borrow_source)"
                .to_owned(),
        ))
        .await;
    // Idempotent: only hydrates rows where new columns are still NULL.
    match backfill_borrow_metadata(db).await {
        Ok(stats) if stats.hydrated + stats.unparsed > 0 => tracing::info!(
            "Migration 075 backfill: hydrated {} borrowed copies, {} left for client-side fallback",
            stats.hydrated,
            stats.unparsed
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("Migration 075 backfill skipped: {e}"),
    }

    // Migration 076: Soft-delete column for notifications. Replaces the
    // previous hard DELETE on dismiss so the dedup check in
    // `check_loan_reminders` (which probes `exists(event_type, ref_type,
    // ref_id)`) keeps seeing a dismissed reminder and does not recreate it
    // on the next 30s poll. NULL = active, ISO 8601 timestamp = dismissed.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE notifications ADD COLUMN dismissed_at TEXT".to_owned(),
        ))
        .await;

    // Migration 077: Bulk metadata gap-fill journal + run state (ADR-041).
    // `metadata_fill_run` persists one row per bulk "Compléter ma bibliothèque"
    // run so progress survives a kill/restart and a run can resume from its
    // cursor. `metadata_fill_journal` records every field this feature wrote so
    // a fill can be undone safely (revert only if the value is still ours).
    for stmt in [
        r#"CREATE TABLE IF NOT EXISTS metadata_fill_run (
            batch_id TEXT PRIMARY KEY,
            status TEXT NOT NULL,
            total INTEGER NOT NULL DEFAULT 0,
            done INTEGER NOT NULL DEFAULT 0,
            filled INTEGER NOT NULL DEFAULT 0,
            skipped INTEGER NOT NULL DEFAULT 0,
            errored INTEGER NOT NULL DEFAULT 0,
            cursor_book_id INTEGER NOT NULL DEFAULT 0,
            current_title TEXT,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
        r#"CREATE TABLE IF NOT EXISTS metadata_fill_journal (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            batch_id TEXT NOT NULL,
            book_id INTEGER NOT NULL,
            field TEXT NOT NULL,
            value_set TEXT NOT NULL,
            created_at TEXT NOT NULL,
            undone_at TEXT
        )"#,
        "CREATE INDEX IF NOT EXISTS idx_mfj_batch ON metadata_fill_journal(batch_id)",
        "CREATE INDEX IF NOT EXISTS idx_mfj_active ON metadata_fill_journal(undone_at, created_at)",
        "CREATE INDEX IF NOT EXISTS idx_mfj_book ON metadata_fill_journal(book_id)",
    ] {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                stmt.to_owned(),
            ))
            .await;
    }

    // Migration 078: Stable per-row identifiers (UUID v7) for the replicated
    // entities (ST-03). Local INTEGER PKs are device-local and cannot correlate
    // the same row across devices — the root cause of the op-replay failure
    // (ADR-011) and a hard prerequisite of the hub E2EE-sync epic (decision D3).
    //
    // Purely additive: add a nullable `uuid TEXT`, backfill existing rows, and
    // enforce uniqueness via an index. Integer PKs and FKs are intentionally
    // left unchanged here — the switch to uuid-as-PK and the FK removal for
    // cr-sqlite happen in ST-05, not in this migration.
    for table in ["books", "copies", "authors", "contacts", "tags", "loans"] {
        // 1. Add the column (idempotent: error ignored if it already exists).
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                format!("ALTER TABLE {table} ADD COLUMN uuid TEXT"),
            ))
            .await;
        // 2. Backfill rows that predate the column.
        backfill_uuids(db, table).await?;
        // 3. Generate a uuid on every future insert that does not already carry
        //    one. The `before_save` ActiveModel hook only fires on `am.insert()`
        //    (and is required there so the RETURNING'd model carries the uuid),
        //    but the codebase also inserts via `Entity::insert(am).exec()` and
        //    raw SQL, which bypass it. This trigger is the catch-all that keeps
        //    every row's uuid non-NULL regardless of the insert path. Generates
        //    a UUID v7 in SQL (timestamp-ordered, same shape as `new_uuid_v7`).
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                format!(
                    "CREATE TRIGGER IF NOT EXISTS trg_{table}_uuid \
                     AFTER INSERT ON {table} FOR EACH ROW WHEN NEW.uuid IS NULL \
                     BEGIN UPDATE {table} SET uuid = {expr} WHERE id = NEW.id; END",
                    expr = uuid_v7_sql_expr()
                ),
            ))
            .await;
        // 4. Enforce global uniqueness. Built after the backfill so the index
        //    never has to reconcile the transient all-NULL state.
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                format!("CREATE UNIQUE INDEX IF NOT EXISTS idx_{table}_uuid ON {table}(uuid)"),
            ))
            .await;
    }

    // Extension modules — migrations 045+
    crate::modules::memory_game::migrate(db).await?;
    crate::modules::sliding_puzzle::migrate(db).await?;
    crate::modules::hangman::migrate(db).await?;
    crate::modules::book_notes::migrate(db).await?;

    // Migration 079: one-shot sweep of rows orphaned by deletions that ran
    // while a pooled connection had `foreign_keys` disabled, so the
    // `ON DELETE CASCADE` to `peers` never fired (TICKET-fk-cascade-orphans).
    // The leak is fixed at the source (the directory-cache insert now isolates
    // its FK-off window to a dedicated connection), but pre-existing orphans
    // must be swept once. Runs after the extension-module tables exist.
    // Idempotent via `_migration_log`; the cleanup itself only removes rows
    // whose `peers` parent is gone, so valid rows are untouched.
    let fk_cleanup_done = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT name FROM _migration_log WHERE name = '079_fk_cascade_orphan_cleanup'"
                .to_owned(),
        ))
        .await
        .ok()
        .flatten()
        .is_some();
    if !fk_cleanup_done {
        cleanup_fk_cascade_orphans(db).await;
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "INSERT INTO _migration_log (name, applied_at) \
                 VALUES ('079_fk_cascade_orphan_cleanup', datetime('now'))"
                    .to_owned(),
            ))
            .await;
    }

    // Migration 080: per-account sync cursors for the account E2EE sync layer
    // (ST-05). One row per account: `pull_cursor` is the hub `change_seq`
    // high-water mark consumed so far, `push_version` is the local cr-sqlite
    // `db_version` up to which our own changes were already pushed. Purely
    // additive and isolated from the replicated entity tables.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"CREATE TABLE IF NOT EXISTS account_sync_state (
            account_id TEXT PRIMARY KEY,
            pull_cursor INTEGER NOT NULL DEFAULT 0,
            push_version INTEGER NOT NULL DEFAULT 0,
            last_synced_at TEXT
        )"#
            .to_owned(),
        ))
        .await;

    Ok(())
}

/// Remove rows orphaned by deletions that ran with SQLite `foreign_keys`
/// disabled, so an `ON DELETE CASCADE` to the `peers` table never fired
/// (TICKET-fk-cascade-orphans). Covers the peer-cascade family: every table
/// whose row is meant to disappear when its `peers` parent is deleted. Each
/// statement removes only rows whose parent is genuinely absent, so valid rows
/// are preserved. The `peer_books` directory cache intentionally uses the
/// `peer_id = 0` sentinel (no matching `peers` row); it is excluded so the
/// cache survives. `copies.lender_peer_id` is `ON DELETE SET NULL`, so its
/// dangling references are nulled rather than the rows deleted.
///
/// Idempotent and safe to run repeatedly. Exposed at crate level so tests can
/// seed orphans and assert the sweep behavior directly.
pub(crate) async fn cleanup_fk_cascade_orphans(db: &DatabaseConnection) {
    const STATEMENTS: [&str; 8] = [
        "DELETE FROM peer_memory_scores WHERE peer_id NOT IN (SELECT id FROM peers)",
        "DELETE FROM peer_puzzle_scores WHERE peer_id NOT IN (SELECT id FROM peers)",
        "DELETE FROM peer_hangman_scores WHERE peer_id NOT IN (SELECT id FROM peers)",
        "DELETE FROM peer_gamification_stats WHERE peer_id NOT IN (SELECT id FROM peers)",
        "DELETE FROM peer_books WHERE peer_id <> 0 AND peer_id NOT IN (SELECT id FROM peers)",
        "DELETE FROM p2p_requests WHERE from_peer_id NOT IN (SELECT id FROM peers)",
        "DELETE FROM p2p_outgoing_requests WHERE to_peer_id NOT IN (SELECT id FROM peers)",
        "UPDATE copies SET lender_peer_id = NULL \
         WHERE lender_peer_id IS NOT NULL AND lender_peer_id NOT IN (SELECT id FROM peers)",
    ];
    for stmt in STATEMENTS {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                stmt.to_owned(),
            ))
            .await;
    }
}

#[cfg(test)]
mod fk_cascade_tests {
    use super::{cleanup_fk_cascade_orphans, init_db};
    use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

    async fn exec(db: &DatabaseConnection, sql: &str) {
        db.execute(Statement::from_string(
            db.get_database_backend(),
            sql.to_owned(),
        ))
        .await
        .unwrap();
    }

    async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                sql.to_owned(),
            ))
            .await
            .unwrap()
            .expect("count query returns a row");
        row.try_get::<i64>("", "c").unwrap()
    }

    /// Acceptance criterion: every connection opened by the core enforces
    /// foreign keys, so `ON DELETE CASCADE` actually fires.
    #[tokio::test]
    async fn init_db_connection_enforces_foreign_keys() {
        let db = init_db("sqlite::memory:").await.unwrap();
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "PRAGMA foreign_keys".to_owned(),
            ))
            .await
            .unwrap()
            .expect("PRAGMA foreign_keys returns a row");
        let enabled: i32 = row.try_get("", "foreign_keys").unwrap();
        assert_eq!(enabled, 1, "core connections must enforce foreign keys");
    }

    /// The cleanup sweep deletes rows whose `peers` parent is gone and leaves
    /// valid rows alone; `foreign_key_check` is empty afterward (real-base
    /// scenario: no directory sentinels present).
    #[tokio::test]
    async fn cleanup_removes_orphans_and_spares_valid_rows() {
        let db = init_db("sqlite::memory:").await.unwrap();
        // Seed with FK off so rows referencing a deleted peer can be inserted.
        exec(&db, "PRAGMA foreign_keys = OFF").await;
        exec(
            &db,
            "INSERT INTO peers (id, name, url) VALUES (1, 'Real', 'http://peer-1')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO peer_memory_scores (peer_id, library_name, best_score, difficulty, played_at, synced_at) \
             VALUES (1, 'Real', 10.0, 'easy', '2026-01-01', '2026-01-01')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO peer_memory_scores (peer_id, library_name, best_score, difficulty, played_at, synced_at) \
             VALUES (999, 'Ghost', 99.0, 'hard', '2026-01-01', '2026-01-01')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO peer_books (peer_id, remote_book_id, title, synced_at) \
             VALUES (1, 5, 'Real book', '2026-01-01')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO peer_books (peer_id, remote_book_id, title, synced_at) \
             VALUES (999, 6, 'Ghost book', '2026-01-01')",
        )
        .await;
        exec(&db, "PRAGMA foreign_keys = ON").await;

        cleanup_fk_cascade_orphans(&db).await;

        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS c FROM peer_memory_scores WHERE peer_id = 1"
            )
            .await,
            1,
            "valid score must be kept",
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS c FROM peer_memory_scores WHERE peer_id = 999"
            )
            .await,
            0,
            "orphan score must be removed",
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS c FROM peer_books WHERE peer_id = 1"
            )
            .await,
            1,
            "valid peer book must be kept",
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS c FROM peer_books WHERE peer_id = 999"
            )
            .await,
            0,
            "orphan peer book must be removed",
        );

        let violations = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "PRAGMA foreign_key_check".to_owned(),
            ))
            .await
            .unwrap();
        assert!(
            violations.is_empty(),
            "no FK violations should remain after cleanup",
        );
    }

    /// The directory cache deliberately stores `peer_id = 0` sentinel rows (no
    /// matching `peers` row). The sweep must never touch them.
    #[tokio::test]
    async fn cleanup_preserves_directory_sentinel() {
        let db = init_db("sqlite::memory:").await.unwrap();
        exec(&db, "PRAGMA foreign_keys = OFF").await;
        exec(
            &db,
            "INSERT INTO peer_books (peer_id, remote_book_id, title, synced_at, node_id) \
             VALUES (0, 0, 'Directory entry', '2026-01-01', 'node-uuid')",
        )
        .await;
        exec(
            &db,
            "INSERT INTO peer_books (peer_id, remote_book_id, title, synced_at) \
             VALUES (999, 6, 'Ghost book', '2026-01-01')",
        )
        .await;
        exec(&db, "PRAGMA foreign_keys = ON").await;

        cleanup_fk_cascade_orphans(&db).await;

        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS c FROM peer_books WHERE peer_id = 0"
            )
            .await,
            1,
            "directory sentinel must survive the sweep",
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS c FROM peer_books WHERE peer_id = 999"
            )
            .await,
            0,
            "orphan peer book must be removed",
        );
    }
}

/// A SQL expression that evaluates to a fresh UUID v7 string (ST-03), used by
/// the per-table AFTER INSERT triggers from migration 078 so that *any* insert
/// path (raw SQL, `Entity::insert(..).exec()`, etc.) gets a stable id without
/// going through the Rust `before_save` hook.
///
/// Layout matches RFC 9562 v7: 48-bit millisecond timestamp, version nibble 7,
/// variant nibble (8/9/a/b), random remainder. `julianday('now')` is constant
/// within a single statement (so both timestamp halves agree), while
/// `randomblob`/`random` re-evaluate per call (so the random bits differ).
fn uuid_v7_sql_expr() -> &'static str {
    "lower(\
        substr(printf('%012x', cast((julianday('now') - 2440587.5) * 86400000.0 as integer)), 1, 8) \
        || '-' || \
        substr(printf('%012x', cast((julianday('now') - 2440587.5) * 86400000.0 as integer)), 9, 4) \
        || '-7' || substr(hex(randomblob(2)), 2, 3) \
        || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)), 2, 3) \
        || '-' || hex(randomblob(6)) \
    )"
}

/// Backfill stable UUIDs (ST-03, migration 078) on every row of `table` whose
/// `uuid` column is still NULL — i.e. rows that existed before the column was
/// added. Runs synchronously inside `run_migrations`, so by the time the
/// connection is handed out every row has a uuid and the SeaORM models can map
/// the column to a non-optional `String`.
///
/// `table` comes only from the migration's own fixed list (never user input),
/// so interpolating it into the SQL is safe.
///
/// Done as a single set-based `UPDATE`: `uuid_v7_sql_expr()` re-evaluates its
/// `randomblob`/`random` parts per row (so every backfilled row gets a distinct
/// uuid), while `julianday('now')` is constant within the statement (so they
/// share a timestamp prefix). Runs before the UNIQUE index is built, and inside
/// `run_migrations`, so by the time the connection is handed out no row's uuid
/// is NULL and the SeaORM models can map the column to a non-optional `String`.
async fn backfill_uuids(db: &DatabaseConnection, table: &str) -> Result<(), DbErr> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        format!(
            "UPDATE {table} SET uuid = {expr} WHERE uuid IS NULL",
            expr = uuid_v7_sql_expr()
        ),
    ))
    .await?;
    Ok(())
}

/// Result of `backfill_borrow_metadata` — used by migration logs and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillStats {
    pub hydrated: usize,
    pub unparsed: usize,
}

/// Parse a legacy free-text `notes` string on a borrowed copy into
/// `(display_name, due_date, source)`. Returns `None` when the string
/// does not match any known pre-ADR-034 format.
///
/// Recognized formats:
/// - `"Emprunté de NAME jusqu'au DATE"` — written by the Rust peer flow.
/// - `"Borrowed from NAME"` / `"Emprunté à NAME"` — written by the Flutter
///   contact-loan flow across locales. An optional legacy `" (ID: N)"`
///   suffix is stripped if present.
pub(crate) fn parse_legacy_borrow_notes(
    notes: &str,
) -> Option<(String, Option<String>, &'static str)> {
    let trimmed = notes.trim();
    if let Some(rest) = trimmed.strip_prefix("Emprunté de ")
        && let Some((name, due)) = rest.split_once(" jusqu'au ")
    {
        let name = name.trim();
        let due = due.trim();
        if !name.is_empty() && !due.is_empty() {
            return Some((name.to_string(), Some(due.to_string()), "peer"));
        }
    }

    for prefix in ["Borrowed from", "Emprunté à"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim_start_matches([':', ' ']).trim();
            if rest.is_empty() {
                continue;
            }
            let name = rest
                .rsplit_once(" (ID: ")
                .map(|(n, _)| n.trim().to_string())
                .unwrap_or_else(|| rest.to_string());
            if !name.is_empty() {
                return Some((name, None, "contact"));
            }
        }
    }

    None
}

/// Hydrate the four ADR-034 columns on existing borrowed copies by parsing
/// the legacy free-text `notes` field. Idempotent: only rows where
/// `lender_display_name IS NULL` are considered, and rows whose `notes`
/// does not match a known format are left untouched (the Flutter fallback
/// still renders them via the old regex).
///
/// Exposed at crate level so migration tests can exercise it directly
/// without rerunning every migration.
pub async fn backfill_borrow_metadata(db: &DatabaseConnection) -> Result<BackfillStats, DbErr> {
    let backend = db.get_database_backend();
    let rows = db
        .query_all(Statement::from_string(
            backend,
            "SELECT id, notes FROM copies \
             WHERE status = 'borrowed' \
               AND notes IS NOT NULL \
               AND lender_display_name IS NULL"
                .to_owned(),
        ))
        .await?;

    let mut stats = BackfillStats::default();
    for row in rows {
        let id: i32 = row.try_get("", "id")?;
        let notes: String = row.try_get("", "notes")?;

        let Some((name, due, source)) = parse_legacy_borrow_notes(&notes) else {
            stats.unparsed += 1;
            continue;
        };

        let stmt = match due {
            Some(due) => Statement::from_sql_and_values(
                backend,
                "UPDATE copies SET lender_display_name = ?, borrow_due_date = ?, borrow_source = ? \
                 WHERE id = ?",
                [
                    sea_orm::Value::from(name),
                    sea_orm::Value::from(due),
                    sea_orm::Value::from(source.to_string()),
                    sea_orm::Value::from(id),
                ],
            ),
            None => Statement::from_sql_and_values(
                backend,
                "UPDATE copies SET lender_display_name = ?, borrow_source = ? WHERE id = ?",
                [
                    sea_orm::Value::from(name),
                    sea_orm::Value::from(source.to_string()),
                    sea_orm::Value::from(id),
                ],
            ),
        };
        db.execute(stmt).await?;
        stats.hydrated += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Migration 078: stable UUIDs (ST-03) ---

    #[tokio::test]
    async fn migration_078_adds_uuid_column_and_unique_index() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        for table in ["books", "copies", "authors", "contacts", "tags", "loans"] {
            let cols = db
                .query_all(Statement::from_string(
                    db.get_database_backend(),
                    format!("PRAGMA table_info({table})"),
                ))
                .await
                .expect("table_info");
            let has_uuid = cols.iter().any(|r| {
                r.try_get::<String>("", "name")
                    .map(|n| n == "uuid")
                    .unwrap_or(false)
            });
            assert!(has_uuid, "table {table} must have a uuid column");

            let idx = db
                .query_all(Statement::from_string(
                    db.get_database_backend(),
                    format!("PRAGMA index_list({table})"),
                ))
                .await
                .expect("index_list");
            let has_idx = idx.iter().any(|r| {
                r.try_get::<String>("", "name")
                    .map(|n| n == format!("idx_{table}_uuid"))
                    .unwrap_or(false)
            });
            assert!(has_idx, "table {table} must have idx_{table}_uuid");
        }
    }

    #[tokio::test]
    async fn backfill_fills_pre_existing_null_uuid_rows() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        // Insert several rows, then force their uuids back to NULL to mimic rows
        // that predate migration 078. (The AFTER INSERT trigger fills uuid on
        // insert, but it does not fire on this UPDATE, so the NULLs stick and
        // the set-based backfill is what must repair them.)
        for stmt in [
            "INSERT INTO authors (name, created_at, updated_at) VALUES \
             ('Old A', '2020-01-01', '2020-01-01'), \
             ('Old B', '2020-01-01', '2020-01-01'), \
             ('Old C', '2020-01-01', '2020-01-01')",
            "UPDATE authors SET uuid = NULL",
        ] {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                stmt.to_owned(),
            ))
            .await
            .expect("seed pre-078 rows");
        }

        backfill_uuids(&db, "authors").await.expect("backfill");

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT uuid FROM authors ORDER BY id".to_owned(),
            ))
            .await
            .expect("select uuids");
        let uuids: Vec<String> = rows
            .iter()
            .map(|r| {
                r.try_get::<String>("", "uuid")
                    .expect("uuid must be non-null")
            })
            .collect();
        assert_eq!(uuids.len(), 3, "all seeded rows must be present");

        // Every backfilled row is a valid v7 uuid...
        for uuid in &uuids {
            assert_eq!(
                uuid::Uuid::parse_str(uuid)
                    .expect("valid uuid")
                    .get_version_num(),
                7,
                "backfilled uuid must be v7"
            );
        }
        // ...and the single set-based UPDATE assigns a DISTINCT uuid per row
        // (randomblob/random re-evaluate per row). This is the property the
        // loop-to-set-UPDATE refactor depends on.
        let unique: std::collections::HashSet<&String> = uuids.iter().collect();
        assert_eq!(
            unique.len(),
            3,
            "each backfilled row must get a distinct uuid"
        );
    }

    #[tokio::test]
    async fn entity_insert_path_gets_uuid_via_trigger() {
        // `Entity::insert(..).exec()` bypasses the `before_save` hook; the
        // AFTER INSERT trigger must still populate uuid so model reads (which
        // map uuid to a non-optional String) never hit NULL. This reproduces
        // the path that initially failed across sync/processor + service tests.
        use crate::models::author;
        use sea_orm::{ActiveValue::NotSet, EntityTrait, Set};

        let db = init_db("sqlite::memory:").await.expect("init db");
        let res = author::Entity::insert(author::ActiveModel {
            id: NotSet,
            uuid: NotSet,
            name: Set("Trigger Test".to_owned()),
            created_at: Set("2020".to_owned()),
            updated_at: Set("2020".to_owned()),
        })
        .exec(&db)
        .await
        .expect("insert via Entity::insert");

        let row = author::Entity::find_by_id(res.last_insert_id)
            .one(&db)
            .await
            .expect("find")
            .expect("row exists");
        assert!(
            !row.uuid.is_empty(),
            "trigger must set uuid on the Entity::insert path"
        );
        assert_eq!(
            uuid::Uuid::parse_str(&row.uuid)
                .expect("valid uuid")
                .get_version_num(),
            7,
            "trigger-generated uuid must be v7"
        );
    }

    #[tokio::test]
    async fn uuid_unique_index_rejects_duplicates() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        let backend = db.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            "INSERT INTO authors (name, uuid, created_at, updated_at) \
             VALUES ('A', 'dup-uuid', '2020', '2020')"
                .to_owned(),
        ))
        .await
        .expect("first insert");
        let dup = db
            .execute(Statement::from_string(
                backend,
                "INSERT INTO authors (name, uuid, created_at, updated_at) \
                 VALUES ('B', 'dup-uuid', '2020', '2020')"
                    .to_owned(),
            ))
            .await;
        assert!(dup.is_err(), "duplicate uuid must violate the unique index");
    }

    #[test]
    fn parse_peer_format() {
        let (name, due, source) =
            parse_legacy_borrow_notes("Emprunté de Alice jusqu'au 2026-12-01").unwrap();
        assert_eq!(name, "Alice");
        assert_eq!(due.as_deref(), Some("2026-12-01"));
        assert_eq!(source, "peer");
    }

    #[test]
    fn parse_peer_format_multiword_name() {
        let (name, due, _) =
            parse_legacy_borrow_notes("Emprunté de Bob l'Éponge jusqu'au 2030-01-15").unwrap();
        assert_eq!(name, "Bob l'Éponge");
        assert_eq!(due.as_deref(), Some("2030-01-15"));
    }

    #[test]
    fn parse_contact_format_english() {
        let (name, due, source) = parse_legacy_borrow_notes("Borrowed from Charlie").unwrap();
        assert_eq!(name, "Charlie");
        assert!(due.is_none());
        assert_eq!(source, "contact");
    }

    #[test]
    fn parse_contact_format_french_a() {
        let (name, _, source) = parse_legacy_borrow_notes("Emprunté à Diane").unwrap();
        assert_eq!(name, "Diane");
        assert_eq!(source, "contact");
    }

    #[test]
    fn parse_contact_format_with_id_suffix() {
        let (name, _, source) = parse_legacy_borrow_notes("Borrowed from: Eve (ID: 42)").unwrap();
        assert_eq!(name, "Eve");
        assert_eq!(source, "contact");
    }

    #[test]
    fn parse_unknown_format_returns_none() {
        assert!(parse_legacy_borrow_notes("Some freeform user note").is_none());
        assert!(parse_legacy_borrow_notes("").is_none());
        assert!(parse_legacy_borrow_notes("Emprunté de ").is_none());
    }
}
