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

async fn run_migrations(db: &DatabaseConnection) -> Result<(), DbErr> {
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
            role TEXT NOT NULL DEFAULT 'user',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
        .to_owned(),
    ))
    .await?;

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

    Ok(())
}
