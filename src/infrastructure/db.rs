use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement};

#[derive(Clone)]
pub struct AppState {
    pub conn: DatabaseConnection,
}

pub async fn init_db(database_url: &str) -> Result<DatabaseConnection, DbErr> {
    let db = Database::connect(database_url).await?;

    // Run migrations manually (simple SQL)
    run_migrations(&db).await?;

    Ok(db)
}

pub(crate) async fn run_migrations(db: &DatabaseConnection) -> Result<(), DbErr> {
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

    // Insert default library config if not exists
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO library_config (id, name, description, tags, latitude, longitude, share_location, created_at, updated_at)
        VALUES (1, 'My Library', 'Personal book collection', '[]', NULL, NULL, 0, datetime('now'), datetime('now'))
        "#
        .to_owned(),
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
    // The subquery pattern:
    //   "duplicate ids" = books whose isbn appears more than once AND whose id
    //   is NOT the MIN(id) for that isbn.

    // Reassign copies to the kept book
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            UPDATE copies SET book_id = (
                SELECT MIN(b2.id) FROM books b2
                WHERE b2.isbn = (SELECT isbn FROM books WHERE id = copies.book_id)
                  AND b2.isbn IS NOT NULL
            )
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books WHERE isbn IS NOT NULL GROUP BY isbn
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
                  AND b2.isbn IS NOT NULL
            )
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books WHERE isbn IS NOT NULL GROUP BY isbn
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
                      AND b2.isbn IS NOT NULL),
                   author_id
            FROM book_authors
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books WHERE isbn IS NOT NULL GROUP BY isbn
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
                    SELECT isbn FROM books WHERE isbn IS NOT NULL
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books WHERE isbn IS NOT NULL GROUP BY isbn
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
                      AND b2.isbn IS NOT NULL),
                   tag_id
            FROM book_tags
            WHERE book_id IN (
                SELECT id FROM books WHERE isbn IN (
                    SELECT isbn FROM books WHERE isbn IS NOT NULL
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books WHERE isbn IS NOT NULL GROUP BY isbn
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
                    SELECT isbn FROM books WHERE isbn IS NOT NULL
                    GROUP BY isbn HAVING COUNT(*) > 1
                ) AND id NOT IN (
                    SELECT MIN(id) FROM books WHERE isbn IS NOT NULL GROUP BY isbn
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

    // Delete the duplicate book rows (keep oldest per ISBN)
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"
            DELETE FROM books WHERE isbn IS NOT NULL AND id NOT IN (
                SELECT MIN(id) FROM books GROUP BY isbn
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

    // Extension modules — migrations 045+
    crate::modules::memory_game::migrate(db).await?;
    crate::modules::sliding_puzzle::migrate(db).await?;
    crate::modules::hangman::migrate(db).await?;
    crate::modules::book_notes::migrate(db).await?;

    Ok(())
}
