use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement};
use sqlx::Row as _;

use crate::utils::default_library_name::compute_default_library_name_seed;

/// Highest migration index applied by `run_migrations`.
///
/// Embedded in `.bgbackup` manifests (ADR-037 §2) so the restore pipeline
/// can decide whether to migrate the archived DB forward or refuse a
/// future-version archive. **Bump this constant whenever a new migration
/// is appended to `run_migrations`.**
pub const SCHEMA_VERSION: u32 = 81;

pub async fn init_db(database_url: &str) -> Result<DatabaseConnection, DbErr> {
    let db = Database::connect(database_url).await?;

    // Run migrations manually (simple SQL)
    run_migrations(&db).await?;

    Ok(db)
}

/// Adaptive database entrypoint for the real app bootstrap (the FFI `init_backend`
/// and the server `main`) on account-sync builds (ADR-044). The mode is chosen at
/// runtime from the database itself — NOT from the compile feature — so a build that
/// merely *can* sync does not CRR-ify every user's database:
///
/// - **Sync mode** (the user enrolled an account, see [`detect_sync_mode`]): open a
///   cr-sqlite-loaded, single-connection pool and promote the replicated tables to
///   CRRs. cr-sqlite keeps per-connection state (site id, db version), so the whole
///   pool is pinned to one physical connection that every query and the merge engine
///   share. The extension is made available before the connection is used — a
///   process-wide auto-extension on the static ship build, or a per-connection load
///   on the dynamic dev build. CRR promotion runs after migrations so each table
///   already has its uuid PK and no foreign keys.
/// - **Default mode** (not enrolled): fall through to plain [`init_db`] — a NORMAL
///   multi-connection pool with no cr-sqlite and no CRRs. Non-sync users pay zero
///   cr-sqlite cost and their database stays a plain, lock-in-free SQLite file that
///   any build can write.
///
/// CRR-ification is therefore gated on enrollment, and reversed by
/// [`crsqlite_crr::teardown_crrs`](crate::infrastructure::crsqlite_crr::teardown_crrs)
/// on logout. Plain [`init_db`] stays the path for tests and the backup/restore
/// subsystem, which must not load cr-sqlite into their transient connections.
#[cfg(feature = "account_sync")]
pub async fn init_db_account_sync(database_url: &str) -> Result<DatabaseConnection, DbErr> {
    use sea_orm::{RuntimeErr, SqlxSqliteConnector};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    let conn_err = |e: sqlx::Error| DbErr::Conn(RuntimeErr::Internal(e.to_string()));

    // Peek (a plain, read-only connection) whether this database is in account-sync
    // mode before deciding how to open it. Closed before the real pool opens so the
    // two never contend on the SQLite file lock.
    let peek = Database::connect(database_url).await?;
    let sync_mode = detect_sync_mode(&peek).await?;
    peek.close().await?;

    if !sync_mode {
        // Not enrolled (or enrolled but awaiting the post-enrollment restart): a
        // NORMAL multi-connection pool with no cr-sqlite and no CRRs.
        return init_db(database_url).await;
    }

    // Static ship build: register the statically-linked extension as a SQLite
    // auto-extension so every connection opened afterwards exposes `crsql_*`.
    // Must run before the cr-sqlite connection is opened.
    #[cfg(feature = "crsqlite-static")]
    crate::infrastructure::crsqlite_static::register();

    let opts = SqliteConnectOptions::from_str(database_url).map_err(conn_err)?;

    // Dynamic dev build: load the vendored extension per connection. cr-sqlite's
    // entry point is non-standard, so it must be named explicitly. Not needed on the
    // static build, where the auto-extension above already applies to every connection.
    #[cfg(all(feature = "crsqlite", not(feature = "crsqlite-static")))]
    let opts = opts.extension_with_entrypoint(
        crate::infrastructure::crsqlite_dynamic::vendored_extension_path(),
        "sqlite3_crsqlite_init",
    );

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .map_err(conn_err)?;
    let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);

    run_migrations(&db).await?;
    crate::infrastructure::crsqlite_crr::setup_crrs(&db).await?;

    Ok(db)
}

/// Decide whether a database must be opened in account-sync (cr-sqlite) mode. True when:
///
/// 1. The replicated tables are already CRRs (a `*__crsql_clock` companion table
///    exists). Once CRR-ified the database MUST be opened with cr-sqlite — plain
///    writes would fail on the CRR triggers that call the extension.
/// 2. An `account_session` row is present. Enrollment is restart-gated: it persists
///    the session row but defers the first `setup_crrs` to the next boot, where this
///    peek flips the database into sync mode.
///
/// Reads only (`sqlite_master` plus a single-row `account_session` probe), so it is
/// safe on a plain connection opened before the real pool. The `account_session`
/// table only exists once migrations have run at least once, so its presence is
/// probed first (a fresh database has neither signal → default mode).
#[cfg(feature = "account_sync")]
async fn detect_sync_mode(db: &DatabaseConnection) -> Result<bool, DbErr> {
    let backend = db.get_database_backend();

    // 1. Any replicated table already promoted to a CRR?
    let clock = db
        .query_one(Statement::from_string(
            backend,
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name LIKE '%\\_\\_crsql\\_clock' ESCAPE '\\') AS present"
                .to_owned(),
        ))
        .await?;
    if let Some(row) = clock
        && row.try_get::<i32>("", "present")? != 0
    {
        return Ok(true);
    }

    // 2. An enrolled session waiting for its post-enrollment restart.
    let has_table = db
        .query_one(Statement::from_string(
            backend,
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'account_session'"
                .to_owned(),
        ))
        .await?;
    if has_table.is_none() {
        return Ok(false);
    }
    let session = db
        .query_one(Statement::from_string(
            backend,
            "SELECT 1 FROM account_session WHERE id = 0".to_owned(),
        ))
        .await?;
    Ok(session.is_some())
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
            entity_id TEXT NOT NULL,
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
            remote_book_id TEXT NOT NULL,
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
    //
    // Skipped once migration 084 has moved the flag into `book_local` (the
    // table's existence is the signal): re-adding the column every launch would
    // just be churn for 084 to drop again. On a fresh DB `book_local` does not
    // exist yet here (084 creates it later in this same pass), so the column is
    // still added and then extracted as before.
    if !table_exists(db, "book_local").await? {
        let _ = db
            .execute(Statement::from_string(
                db.get_database_backend(),
                "ALTER TABLE books ADD COLUMN hub_cover_upload_failed_at TEXT".to_owned(),
            ))
            .await;
    }

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
    // entities. Local INTEGER PKs are device-local and cannot correlate
    // the same row across devices — the root cause of the op-replay failure
    // (ADR-011) and a hard prerequisite of the hub E2EE-sync epic (decision D3).
    //
    // Purely additive: add a nullable `uuid TEXT`, backfill existing rows, and
    // enforce uniqueness via an index. Integer PKs and FKs are intentionally
    // left unchanged here — the switch to uuid-as-PK and the FK removal for
    // cr-sqlite happen in the account-sync work, not in this migration.
    //
    // Skipped once `migrate_uuid_pk` has flipped the schema (detected by the
    // absence of the integer `id` column on `books`, the same signal that
    // migration gates on). Post-flip this whole block is obsolete: `uuid` is the
    // PK, the backfill is a no-op, and the AFTER-INSERT trigger is dormant. Most
    // importantly it must NOT re-create the `idx_<table>_uuid` UNIQUE index,
    // which cr-sqlite forbids on a CRR and the rebuild deliberately dropped.
    if table_has_column(db, "books", "id").await? {
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
    }

    // Extension modules — migrations 045+
    crate::modules::memory_game::migrate(db).await?;
    crate::modules::sliding_puzzle::migrate(db).await?;
    crate::modules::hangman::migrate(db).await?;
    crate::modules::book_notes::migrate(db).await?;

    // Migration 079: one-shot sweep of rows orphaned by deletions that ran
    // while a pooled connection had `foreign_keys` disabled, so the
    // `ON DELETE CASCADE` to `peers` never fired (the foreign-key cascade-orphan bug).
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

    // Migration 080: per-account sync cursors for the account E2EE sync layer.
    // One row per account: `pull_cursor` is the hub `change_seq`
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
            registry_seq INTEGER NOT NULL DEFAULT 0,
            last_synced_at TEXT
        )"#
            .to_owned(),
        ))
        .await;

    // Additive: `registry_seq` is the last adopted signed-registry version, used as the
    // anti-rollback floor for H3 device-registry adoption. A table created before this
    // column existed gets it here; the ALTER is a no-op (ignored error) once present.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE account_sync_state ADD COLUMN registry_seq INTEGER NOT NULL DEFAULT 0"
                .to_owned(),
        ))
        .await;

    // Migration 081: client at-rest account session (ST-05 Phase F, ADR-042 §14
    // client-persistence addendum). Singleton row (`id = 0`, one account per device in
    // v1) holding the unlocked trousseau SEALED at rest under the Argon2(library_uuid)
    // device-local key (same root that protects `crypto_keys`), plus the opaque hub
    // `account_id`, the login `email`, and this device's random base64url `device_id`
    // lane key. `encrypted_bundle` is `nonce || ciphertext` from `seal_at_rest`; the
    // trousseau plaintext is never stored. Cleared on logout. Purely additive.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"CREATE TABLE IF NOT EXISTS account_session (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            account_id TEXT NOT NULL,
            email TEXT NOT NULL,
            device_id TEXT NOT NULL,
            encrypted_bundle BLOB NOT NULL,
            salt BLOB NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"#
            .to_owned(),
        ))
        .await;

    // Migration 082: id INTEGER -> uuid PRIMARY KEY on the replicated entities
    // (ADR-044 Addendum A + B). Runs last, once all tables (incl. module tables
    // such as `book_notes`) exist. Gated on the still-present integer `id`, so it
    // is a no-op once applied. This is the irreversible spine of the account-sync
    // epic; see `migrate_uuid_pk` for the mechanics and risk controls.
    migrate_uuid_pk(db).await?;

    // Migration 083: per-lane rollback-detection state for the account E2EE sync
    // layer (ADR-042 §14 / ADR-044 §7). One row per lane keyed by
    // `(account_id, opaque_id, device_id)`, holding the highest in-ciphertext HLC
    // applied for that lane. On pull a blob whose HLC does not advance past
    // `last_hlc` is rejected as a stale replay (a hostile hub re-serving an
    // old-but-valid blob passes the AEAD but cannot pass this check). Purely
    // additive and isolated from the replicated entity tables, so it is safe to
    // create after the uuid-PK rebuild above (it is not a rebuild target).
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"CREATE TABLE IF NOT EXISTS account_lane_hlc (
            account_id TEXT NOT NULL,
            opaque_id TEXT NOT NULL,
            device_id TEXT NOT NULL,
            last_hlc INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT,
            PRIMARY KEY (account_id, opaque_id, device_id)
        )"#
            .to_owned(),
        ))
        .await;

    // Migration 084: extract the device-local `hub_cover_upload_failed_at` flag
    // off `books` into a sibling regular (non-CRR) table `book_local`. This must
    // happen before `books` becomes a cr-sqlite CRR (ADR-044): cr-sqlite
    // replicates every non-PK column with no per-column opt-out,
    // and this negative per-device retry timestamp must stay local (replicating
    // it would conflate two devices' upload states into false "upload failed"
    // badges). `book_local` is intentionally NOT a CRR.
    //
    // Idempotent: the `book_local` create is gateless, and the column is dropped
    // only while it is still present on `books`. A recent-SQLite `DROP COLUMN`
    // (>= 3.35) does the extraction cheaply, with no table rebuild. By this
    // point `migrate_uuid_pk` above has already given `books` its `uuid` PK, so
    // the backfill keys `book_local` on `books.uuid`.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"CREATE TABLE IF NOT EXISTS book_local (
            book_uuid TEXT PRIMARY KEY NOT NULL,
            hub_cover_upload_failed_at TEXT
        )"#
            .to_owned(),
        ))
        .await;

    if table_has_column(db, "books", "hub_cover_upload_failed_at").await? {
        // Preserve pending flags before dropping the column.
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT OR IGNORE INTO book_local (book_uuid, hub_cover_upload_failed_at) \
             SELECT uuid, hub_cover_upload_failed_at FROM books \
             WHERE hub_cover_upload_failed_at IS NOT NULL"
                .to_owned(),
        ))
        .await?;
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE books DROP COLUMN hub_cover_upload_failed_at".to_owned(),
        ))
        .await?;
    }

    // Migration 085: device-local dedup state for custom-cover transport
    // (ADR-046). One row per book holding the cover file's last-synced mtime, so
    // the periodic auto-sync re-encodes and re-uploads a cover only when it
    // actually changed (and never bounces back a cover received from another
    // device). Like `book_local`, it is intentionally NOT a CRR: it records what
    // THIS device transported, which is meaningless to replicate. Gateless and
    // additive, so it is safe to create after the uuid-PK rebuild.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            r#"CREATE TABLE IF NOT EXISTS cover_sync_state (
            book_uuid TEXT PRIMARY KEY NOT NULL,
            file_mtime INTEGER NOT NULL
        )"#
            .to_owned(),
        ))
        .await;

    // Migration 086: RFC3339 instant of the last catalog push confirmed by
    // the hub (200 or 304). The ADR-027 local hash fast path used to skip
    // the HTTP round-trip forever when the catalog never changed, so the
    // hub's cached-catalog TTL (7 days) was never refreshed and the
    // directory fallback went permanently empty for unchanged libraries.
    // push_catalog now bypasses the fast path once this timestamp goes
    // stale, letting the hub bump its TTL via a cheap 304. NULL (legacy
    // installs) counts as stale, so the first sync after this migration
    // re-pushes once and establishes the baseline.
    let _ = db
        .execute(Statement::from_string(
            db.get_database_backend(),
            "ALTER TABLE hub_directory_config ADD COLUMN last_catalog_pushed_at TEXT".to_owned(),
        ))
        .await;

    Ok(())
}

/// True if a table named `name` exists in the main schema.
async fn table_exists(db: &DatabaseConnection, name: &str) -> Result<bool, DbErr> {
    let row = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT 1 AS x FROM sqlite_master WHERE type = 'table' AND name = ?",
            [name.into()],
        ))
        .await?;
    Ok(row.is_some())
}

/// True if `table` currently has a column named `column` (via `PRAGMA
/// table_info`). Used to make column-dropping migrations idempotent.
///
/// `table` is interpolated into the PRAGMA (SQLite cannot bind an identifier
/// there), so callers MUST pass a trusted, hard-coded table name — never
/// user-controlled input.
async fn table_has_column(
    db: &DatabaseConnection,
    table: &str,
    column: &str,
) -> Result<bool, DbErr> {
    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            format!("PRAGMA table_info(\"{table}\")"),
        ))
        .await?;
    for r in &rows {
        if let Ok(name) = r.try_get::<String>("", "name")
            && name == column
        {
            return Ok(true);
        }
    }
    Ok(false)
}

// =========================================================================
// id INTEGER -> uuid PRIMARY KEY migration (ADR-044 Addendum A + B).
//
// The account-sync merge engine (cr-sqlite) replicates whole rows keyed by the
// PRIMARY KEY, so the PK must be the cross-device-stable `uuid` (migration 078),
// not the device-local autoincrement `id` (A's `id=5` is not B's `id=5`). This
// migration drops the integer `id` on the six replicated entity tables, promotes
// `uuid` to PRIMARY KEY, and rewrites every cross-entity reference from integer
// id to the parent's uuid. FK enforcement is removed as a side effect (cr-sqlite
// forbids FK on CRRs); SQLite has no `DROP CONSTRAINT`, so the PK switch and the
// FK removal are the same table rebuild, done together.
//
// This is the highest-regression, effectively one-way step of the epic. It is a
// one-shot rebuild gated on the still-present integer `id` (so it is a no-op once
// applied and on uuid-native fresh installs), runs inside a single transaction
// on a dedicated connection with FK enforcement scoped off, and validates the
// final schema with `PRAGMA foreign_key_check` before committing.
//
// NB: `crsql_as_crr` is intentionally NOT applied here. The uuid-PK, FK-removed
// schema is valid plain SQLite; turning the tables into CRRs needs a cr-sqlite
// loaded connection and belongs to the static-link release step (ADR-044 §2).
// =========================================================================

/// The six replicated entity tables, rebuilt to a `uuid` PRIMARY KEY.
const UUID_REBUILT_ENTITIES: &[&str] = &["books", "authors", "tags", "contacts", "copies", "loans"];

/// One table to rebuild. The plan is generic (driven by `PRAGMA table_info`) so
/// it survives column drift: a column added to a table later is carried over
/// unchanged without touching this plan, as long as it is not a new reference
/// INTO a rebuilt entity (which the fan-out drift guard below catches).
struct UuidRebuildSpec {
    table: &'static str,
    /// Drop the integer `id` column (mode A: entities).
    drop_id: bool,
    /// Promote `uuid` to PRIMARY KEY (mode A: entities).
    uuid_pk: bool,
    /// Composite PK columns (mode B: junctions); empty otherwise.
    composite: &'static [&'static str],
    /// `(column, parent_table)` refs rewritten from integer id to the parent uuid.
    refs: &'static [(&'static str, &'static str)],
    /// Columns dropped from the rebuilt table (extracted to a sibling table).
    drop_cols: &'static [&'static str],
    /// Whether this table becomes a cr-sqlite CRR (account-sync replicated).
    /// CRRs may not carry a non-PK UNIQUE index, so those indexes are NOT
    /// replayed for these tables (the redundant migration-078 `uuid` UNIQUE
    /// index, in particular). Local tables (`sales`, `book_notes`) keep theirs.
    ///
    /// The set of `crr: true` tables MUST match `crsqlite_crr::CRR_TABLES` (the
    /// list `setup_crrs` calls `crsql_as_crr` on); the `crrs_set_up_*` test
    /// guards that coupling against the real migrated schema.
    crr: bool,
}

/// The migration plan. Order is irrelevant for correctness: every `_new` table is
/// populated by resolving references against the still-intact originals (phase 1),
/// and only then are the originals dropped and the `_new` tables renamed (phase 2).
///
/// `book_notes` lives in an extension module (`src/modules/book_notes`); it is a
/// LOCAL table (keeps its integer id) whose `book_id` ref is rewritten to the
/// books uuid. Any new reference INTO a rebuilt entity that is missing here is
/// caught by `uuid_fanout_uncovered` before the destructive phase runs.
fn uuid_rebuild_specs() -> Vec<UuidRebuildSpec> {
    vec![
        // Mode A: entities -> uuid PK, integer id dropped.
        UuidRebuildSpec {
            table: "books",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[],
            // The device-local `hub_cover_upload_failed_at` is NOT dropped here:
            // it is extracted to the sibling `book_local` table by migration 084
            // (a standalone `DROP COLUMN`, ADR-044), which runs
            // after this rebuild. Keeping the rebuild generic avoids a
            // `book_local` read/write rework inside the id-type flip.
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "authors",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[],
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "tags",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[("parent_id", "tags")],
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "contacts",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[],
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "copies",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[("book_id", "books")],
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "loans",
            drop_id: true,
            uuid_pk: true,
            composite: &[],
            refs: &[("copy_id", "copies"), ("contact_id", "contacts")],
            drop_cols: &[],
            crr: true,
        },
        // `collections` already has a TEXT uuid `id` PK (no integer id to drop),
        // so it is NOT an id->uuid conversion and is absent from
        // UUID_REBUILT_ENTITIES. It is rebuilt here only to gain the DEFAULTs
        // that cr-sqlite requires on its NOT NULL columns before it can become a
        // CRR (ADR-044); `crr_default_clause` synthesizes them. `id` keeps its
        // PK, values are preserved, so `collection_books.collection_id` refs stay
        // valid.
        UuidRebuildSpec {
            table: "collections",
            drop_id: false,
            uuid_pk: false,
            composite: &[],
            refs: &[],
            drop_cols: &[],
            crr: true,
        },
        // Mode B: junctions -> composite PK of the rewritten references.
        UuidRebuildSpec {
            table: "book_authors",
            drop_id: false,
            uuid_pk: false,
            composite: &["book_id", "author_id"],
            refs: &[("book_id", "books"), ("author_id", "authors")],
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "book_tags",
            drop_id: false,
            uuid_pk: false,
            composite: &["book_id", "tag_id"],
            refs: &[("book_id", "books"), ("tag_id", "tags")],
            drop_cols: &[],
            crr: true,
        },
        UuidRebuildSpec {
            table: "collection_books",
            drop_id: false,
            uuid_pk: false,
            composite: &["collection_id", "book_id"],
            refs: &[("book_id", "books")],
            drop_cols: &[],
            crr: true,
        },
        // Mode C: local (non-CRR) tables keeping their integer id, but referencing
        // now-uuid-keyed parents -> only their reference columns move to uuid.
        UuidRebuildSpec {
            table: "sales",
            drop_id: false,
            uuid_pk: false,
            composite: &[],
            refs: &[("copy_id", "copies"), ("contact_id", "contacts")],
            drop_cols: &[],
            crr: false,
        },
        UuidRebuildSpec {
            table: "book_notes",
            drop_id: false,
            uuid_pk: false,
            composite: &[],
            refs: &[("book_id", "books")],
            drop_cols: &[],
            crr: false,
        },
    ]
}

fn map_sqlx(e: sqlx::Error) -> DbErr {
    DbErr::Custom(format!("uuid_pk migration: {e}"))
}

/// `(name, type, notnull, pk)` for every column of a table.
async fn uuid_columns(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<Vec<(String, String, bool, bool, Option<String>)>, DbErr> {
    let rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\")"))
        .fetch_all(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>("name"),
                r.get::<String, _>("type"),
                r.get::<i64, _>("notnull") != 0,
                r.get::<i64, _>("pk") != 0,
                // Preserve the column's DEFAULT so the rebuilt table keeps it
                // (PRAGMA `dflt_value` is the raw SQL literal, e.g. `0`, `'to_read'`).
                r.get::<Option<String>, _>("dflt_value"),
            )
        })
        .collect())
}

/// Every foreign key in the DB that points INTO a rebuilt entity table, as
/// `(child_table, child_col, parent_table)`.
async fn uuid_fanout_into_rebuilt(
    conn: &mut sqlx::SqliteConnection,
) -> Result<Vec<(String, String, String)>, DbErr> {
    let tables = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(map_sqlx)?;

    let mut fanout: Vec<(String, String, String)> = Vec::new();
    for t in &tables {
        let name: String = t.get("name");
        let fks = sqlx::query(&format!("PRAGMA foreign_key_list(\"{name}\")"))
            .fetch_all(&mut *conn)
            .await
            .map_err(map_sqlx)?;
        for fk in &fks {
            let parent: String = fk.get("table");
            if UUID_REBUILT_ENTITIES.contains(&parent.as_str()) {
                fanout.push((name.clone(), fk.get::<String, _>("from"), parent));
            }
        }
    }
    fanout.sort();
    Ok(fanout)
}

/// FK references INTO a rebuilt entity that the plan does NOT rewrite. A non-empty
/// result means a table (often an extension module) references a rebuilt entity by
/// integer id and would be left dangling — the exact omission that would corrupt
/// the live migration. The migration aborts loudly rather than rebuild blindly.
fn uuid_fanout_uncovered(
    fanout: &[(String, String, String)],
    specs: &[UuidRebuildSpec],
) -> Vec<String> {
    let handled: std::collections::BTreeSet<(String, String, String)> = specs
        .iter()
        .flat_map(|s| {
            s.refs
                .iter()
                .filter(|(_, parent)| UUID_REBUILT_ENTITIES.contains(parent))
                .map(|(col, parent)| (s.table.to_string(), col.to_string(), parent.to_string()))
        })
        .collect();
    fanout
        .iter()
        .filter(|fk| !handled.contains(*fk))
        .map(|(c, col, p)| format!("{c}.{col} -> {p}"))
        .collect()
}

/// The ` DEFAULT <x>` clause for a rebuilt column.
///
/// cr-sqlite requires every NOT NULL, non-PK column of a CRR to carry a DEFAULT
/// (a row synthesized during merge before all of its columns have arrived must
/// still satisfy NOT NULL). This returns the column's own default if it has
/// one; otherwise, for a NOT NULL non-PK column, a type-appropriate placeholder
/// (`0` for numeric, `x''` for blobs, `''` otherwise) which the real merged
/// value always overwrites; otherwise the empty string. PK columns are exempt
/// (cr-sqlite checks `pk = 0`), so they never get a synthesized default.
fn crr_default_clause(dflt: &Option<String>, notnull: bool, is_pk: bool, ty: &str) -> String {
    if let Some(d) = dflt {
        return format!(" DEFAULT {d}");
    }
    if !notnull || is_pk {
        return String::new();
    }
    let t = ty.to_ascii_uppercase();
    let placeholder = if t.contains("INT")
        || t.contains("REAL")
        || t.contains("FLOA")
        || t.contains("DOUB")
        || t.contains("NUM")
        || t.contains("DEC")
    {
        "0"
    } else if t.contains("BLOB") {
        "x''"
    } else {
        "''"
    };
    format!(" DEFAULT {placeholder}")
}

/// Phase 1: build and populate `<table>__new`, resolving refs against the intact
/// original via LEFT JOIN to the parent's uuid.
async fn uuid_build_new(
    conn: &mut sqlx::SqliteConnection,
    spec: &UuidRebuildSpec,
) -> Result<(), DbErr> {
    let cols = uuid_columns(conn, spec.table).await?;
    let mut defs: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut sel: Vec<String> = Vec::new();
    let mut joins = String::new();

    for (name, ty, notnull, pk, dflt) in &cols {
        if name == "id" && spec.drop_id {
            continue;
        }
        if spec.drop_cols.contains(&name.as_str()) {
            continue;
        }
        if name == "uuid" {
            // The migration-078 AFTER INSERT trigger that minted uuids cannot
            // survive here: once `uuid` is a NOT NULL PRIMARY KEY, an insert that
            // omits it violates the constraint *before* any AFTER INSERT trigger
            // runs. A column DEFAULT is evaluated at insert time, so it covers
            // every path that bypasses the Rust `before_save` hook
            // (`Entity::insert(..).exec()`, raw SQL); `am.insert()` still
            // overrides it with the app-generated v7 uuid.
            defs.push(format!(
                "uuid TEXT NOT NULL DEFAULT ({expr}){pk}",
                expr = uuid_v7_sql_expr(),
                pk = if spec.uuid_pk { " PRIMARY KEY" } else { "" }
            ));
            names.push("uuid".to_string());
            sel.push("t.uuid".to_string());
            continue;
        }
        if let Some((_, parent)) = spec.refs.iter().find(|(c, _)| c == name) {
            let is_pk = spec.composite.iter().any(|c| c == name);
            defs.push(format!(
                "\"{name}\" TEXT{}{}",
                if *notnull { " NOT NULL" } else { "" },
                crr_default_clause(dflt, *notnull, is_pk, "TEXT")
            ));
            names.push(format!("\"{name}\""));
            let alias = format!("p_{name}");
            sel.push(format!("{alias}.uuid"));
            joins.push_str(&format!(
                " LEFT JOIN \"{parent}\" {alias} ON {alias}.id = t.\"{name}\""
            ));
            continue;
        }
        // Plain column (includes the integer `id` in mode C, which keeps its PK).
        let keep_pk = *pk && !spec.drop_id && !spec.uuid_pk && spec.composite.is_empty();
        let ty = if ty.is_empty() { "TEXT" } else { ty.as_str() };
        let is_pk = keep_pk || spec.composite.iter().any(|c| c == name);
        defs.push(format!(
            "\"{name}\" {ty}{}{}{}",
            if *notnull { " NOT NULL" } else { "" },
            if keep_pk { " PRIMARY KEY" } else { "" },
            crr_default_clause(dflt, *notnull, is_pk, ty)
        ));
        names.push(format!("\"{name}\""));
        sel.push(format!("t.\"{name}\""));
    }

    if !spec.composite.is_empty() {
        let pk_cols = spec
            .composite
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        defs.push(format!("PRIMARY KEY ({pk_cols})"));
    }

    let new = format!("{}__new", spec.table);
    sqlx::query(&format!("CREATE TABLE \"{new}\" ({})", defs.join(", ")))
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    sqlx::query(&format!(
        "INSERT INTO \"{new}\" ({}) SELECT {} FROM \"{}\" t{joins}",
        names.join(", "),
        sel.join(", "),
        spec.table
    ))
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// True if a captured `CREATE INDEX` statement is a UNIQUE index (which
/// cr-sqlite forbids on a CRR). Matches the `CREATE UNIQUE INDEX` keyword
/// precisely so an ordinary index on a column merely named `*unique*` is kept.
fn is_unique_index_sql(sql: &str) -> bool {
    sql.trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE UNIQUE INDEX")
}

/// The `CREATE INDEX` statements of a table (user indexes only — the implicit PK
/// index has a NULL `sql`), captured before the table is dropped so they can be
/// replayed on the rebuilt table.
async fn uuid_capture_indexes(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
) -> Result<Vec<String>, DbErr> {
    let rows = sqlx::query(
        "SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await
    .map_err(map_sqlx)?;
    Ok(rows.iter().map(|r| r.get::<String, _>("sql")).collect())
}

/// Promote the integer `id` PRIMARY KEY to the stable `uuid` on the replicated
/// entity tables and rewrite every cross-entity reference to uuid (ADR-044
/// Addendum A/B). Idempotent: a no-op once applied (or on a uuid-native install),
/// detected by the absence of the integer `id` column on `books`.
///
/// Runs on a dedicated pooled connection so the FK-enforcement toggle never
/// leaks to another connection (see `seaorm_pragma_per_connection_pool_leak`),
/// inside one transaction validated by `PRAGMA foreign_key_check` before commit.
pub async fn migrate_uuid_pk(db: &DatabaseConnection) -> Result<(), DbErr> {
    let pool = db.get_sqlite_connection_pool();
    let mut conn = pool.acquire().await.map_err(map_sqlx)?;

    // Gate: skip if `books` no longer carries the integer `id` (already migrated,
    // or a uuid-native fresh install).
    let books_cols = uuid_columns(&mut conn, "books").await?;
    if !books_cols.iter().any(|(n, _, _, _, _)| n == "id") {
        return Ok(());
    }

    let specs = uuid_rebuild_specs();

    // Drift guard (pre-flight, before any destructive op): every FK into a rebuilt
    // entity must be in the plan, or a child would be left dangling.
    let fanout = uuid_fanout_into_rebuilt(&mut conn).await?;
    let uncovered = uuid_fanout_uncovered(&fanout, &specs);
    if !uncovered.is_empty() {
        return Err(DbErr::Custom(format!(
            "uuid_pk migration aborted: unhandled FK(s) into the rebuilt tables \
             (extend uuid_rebuild_specs before migrating): {uncovered:?}"
        )));
    }

    // Resolve the on-disk covers directory now (before the pragma toggles), so
    // the local cover files can be renamed `<id>.jpg` -> `<uuid>.jpg` after the
    // rebuild commits. `None` for an in-memory / unnamed DB (tests, the
    // WS2_REAL_DB temp copy), where the rename is simply skipped.
    let covers_dir = uuid_covers_dir(&mut conn).await;

    // SQLite's table-redefinition procedure. `foreign_keys` cannot change inside a
    // transaction, so it is toggled around it. `legacy_alter_table=ON` stops modern
    // SQLite (>= 3.25) from rewriting references in other objects on RENAME (which
    // would re-validate their FKs against an old-shaped sibling and raise "foreign
    // key mismatch"); the final schema is validated by `foreign_key_check` instead.
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    sqlx::query("PRAGMA legacy_alter_table = ON")
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;

    let result = run_uuid_rebuild(&mut conn, &specs).await;

    // Restore connection pragmas regardless of outcome, before releasing it.
    let _ = sqlx::query("PRAGMA legacy_alter_table = OFF")
        .execute(&mut *conn)
        .await;
    let _ = sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await;
    drop(conn);

    // The rebuild committed; now apply the on-disk cover renames. Done AFTER
    // commit so a rolled-back rebuild never leaves files renamed out from under
    // a `cover_url` that reverted. Best-effort + idempotent per file.
    let cover_renames = result?;
    if let Some(dir) = covers_dir {
        for (old, new) in &cover_renames {
            uuid_apply_cover_rename(&dir, old, new);
        }
    }
    Ok(())
}

/// Resolve the on-disk covers directory (sibling of the SQLite database file),
/// mirroring the registration in `api::frb`. Returns `None` for an in-memory or
/// unnamed database (tests, the `WS2_REAL_DB` temp copy with no `covers/`
/// sibling), so the cover-file rename is simply skipped there.
async fn uuid_covers_dir(conn: &mut sqlx::SqliteConnection) -> Option<std::path::PathBuf> {
    let rows = sqlx::query("PRAGMA database_list")
        .fetch_all(&mut *conn)
        .await
        .ok()?;
    let file = rows.iter().find_map(|r| {
        let name: String = r.get("name");
        let file: String = r.get("file");
        (name == "main" && !file.is_empty()).then_some(file)
    })?;
    std::path::Path::new(&file)
        .parent()
        .map(|p| p.join("covers"))
}

/// Rename a local cover file `<covers_dir>/<old>` to `<covers_dir>/<new>` after
/// the uuid migration commits. Best-effort + idempotent: a missing source or an
/// already-present target is skipped (a genuinely missing cover renders a
/// placeholder, recoverable by re-setting it). `old`/`new` are single-component
/// basenames (`<id>.jpg` / `<uuid>.jpg`), so the join is traversal-safe.
fn uuid_apply_cover_rename(covers_dir: &std::path::Path, old: &str, new: &str) {
    let from = covers_dir.join(old);
    let to = covers_dir.join(new);
    if from.exists()
        && !to.exists()
        && let Err(e) = std::fs::rename(&from, &to)
    {
        tracing::warn!("uuid_pk: cover rename {old} -> {new} failed: {e}");
    }
}

/// One `PRAGMA foreign_key_check` violation: (child table, child rowid — NULL
/// for WITHOUT ROWID tables, referenced parent table, fk index). Snapshotted
/// before and after the uuid rebuild so the integrity gate can single out the
/// violations the rebuild itself introduced.
async fn fk_check_snapshot(
    conn: &mut sqlx::SqliteConnection,
) -> Result<std::collections::HashSet<(String, Option<i64>, String, i64)>, DbErr> {
    let rows = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<String, _>(0),
                r.get::<Option<i64>, _>(1),
                r.get::<String, _>(2),
                r.get::<i64, _>(3),
            )
        })
        .collect())
}

/// The transactional body of `migrate_uuid_pk`. Wraps `uuid_rebuild_inner` in a
/// transaction so a failure (e.g. a `foreign_key_check` violation) rolls back the
/// whole rebuild, leaving the integer `id` in place for a safe retry next launch.
async fn run_uuid_rebuild(
    conn: &mut sqlx::SqliteConnection,
    specs: &[UuidRebuildSpec],
) -> Result<Vec<(String, String)>, DbErr> {
    // Snapshot the violations that already exist BEFORE the rebuild, so the
    // integrity gate at the end only aborts on ones the rebuild introduced.
    let preexisting = fk_check_snapshot(conn).await?;
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    match uuid_rebuild_inner(conn, specs, &preexisting).await {
        Ok(cover_renames) => {
            sqlx::query("COMMIT")
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx)?;
            Ok(cover_renames)
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(e)
        }
    }
}

/// The destructive rebuild itself (no transaction control — see `run_uuid_rebuild`).
///
/// Returns the local cover-file renames (`<id>.jpg` -> `<uuid>.jpg`) the caller
/// must apply on disk AFTER the transaction commits.
async fn uuid_rebuild_inner(
    conn: &mut sqlx::SqliteConnection,
    specs: &[UuidRebuildSpec],
    preexisting_fk_violations: &std::collections::HashSet<(String, Option<i64>, String, i64)>,
) -> Result<Vec<(String, String)>, DbErr> {
    // Phase 1: capture indexes + build every `_new` from intact originals.
    let mut indexes: Vec<String> = Vec::new();
    for spec in specs {
        for sql in uuid_capture_indexes(conn, spec.table).await? {
            // cr-sqlite forbids a non-PK UNIQUE index on a CRR, so they are not
            // replayed for CRR tables. The migration-078 `uuid` UNIQUE index in
            // particular is redundant once `uuid` is the PK. Non-unique indexes
            // (and all indexes on local tables) are kept.
            if spec.crr && is_unique_index_sql(&sql) {
                continue;
            }
            indexes.push(sql);
        }
        uuid_build_new(conn, spec).await?;
    }

    // Phase 2a: drop ALL originals first, so no surviving table references an
    // old-shaped parent when we rename.
    for spec in specs {
        sqlx::query(&format!("DROP TABLE \"{}\"", spec.table))
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx)?;
    }
    // Phase 2b: rename `_new` into place, then replay the captured indexes.
    for spec in specs {
        sqlx::query(&format!(
            "ALTER TABLE \"{}__new\" RENAME TO \"{}\"",
            spec.table, spec.table
        ))
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    }
    for sql in &indexes {
        sqlx::query(sql)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx)?;
    }

    // Normalize `cover_url` to a device-independent value so it can replicate
    // without one device clobbering another (ADR-044 Addendum A.4), and re-key
    // local custom covers from `<old id>.jpg` to `<uuid>.jpg` so the resolver
    // finds them by the book's new uuid identity (S4d). The matching on-disk
    // file renames are collected and returned to be applied after commit.
    let mut cover_renames: Vec<(String, String)> = Vec::new();
    let rows = sqlx::query("SELECT uuid, cover_url FROM books WHERE cover_url IS NOT NULL")
        .fetch_all(&mut *conn)
        .await
        .map_err(map_sqlx)?;
    for row in &rows {
        let uuid: String = row.get("uuid");
        let current: String = row.get("cover_url");
        let (stored, rename) = crate::utils::cover_url::plan_cover_migration(Some(&current), &uuid);
        if let Some(rename) = rename {
            cover_renames.push(rename);
        }
        if let Some(stored) = stored
            && stored != current
        {
            sqlx::query("UPDATE books SET cover_url = ?1 WHERE uuid = ?2")
                .bind(stored)
                .bind(uuid)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx)?;
        }
    }

    // Integrity gate: the rebuild must not introduce any NEW foreign-key
    // violation. Compared against the pre-rebuild snapshot rather than
    // asserting an empty report, because a real library legitimately carries
    // FK-violating rows that predate the flip: the hub-directory cache keeps
    // `peer_books.peer_id = 0` sentinel rows (written in a dedicated FK-off
    // window, see `upsert_directory_catalog_cache`), plus possible orphans
    // from the era of the FK-off cascade bug. Those involve only non-rebuilt
    // tables and are the pre-flip status quo, not a rebuild failure — aborting
    // on them made init fail on every launch for any device with a populated
    // directory cache.
    let post = fk_check_snapshot(conn).await?;
    let mut introduced: Vec<_> = post.difference(preexisting_fk_violations).collect();
    if !introduced.is_empty() {
        introduced.sort();
        let sample = introduced
            .iter()
            .take(5)
            .map(|(child, _, parent, _)| format!("{child} -> {parent}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(DbErr::Custom(format!(
            "uuid_pk migration aborted: rebuild introduced {} foreign-key violation(s) (e.g. {sample})",
            introduced.len()
        )));
    }
    Ok(cover_renames)
}

/// Remove rows orphaned by deletions that ran with SQLite `foreign_keys`
/// disabled, so an `ON DELETE CASCADE` to the `peers` table never fired
/// (the foreign-key cascade-orphan bug). Covers the peer-cascade family: every table
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

/// A SQL expression that evaluates to a fresh UUID v7 string, used by
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

/// Backfill stable UUIDs (migration 078) on every row of `table` whose
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
            // Key by `uuid` (present since migration 078) so this works whether or
            // not the integer `id` column still exists (it is dropped once the
            // uuid-PK migration has run).
            "SELECT uuid, notes FROM copies \
             WHERE status = 'borrowed' \
               AND notes IS NOT NULL \
               AND lender_display_name IS NULL"
                .to_owned(),
        ))
        .await?;

    let mut stats = BackfillStats::default();
    for row in rows {
        let id: String = row.try_get("", "uuid")?;
        let notes: String = row.try_get("", "notes")?;

        let Some((name, due, source)) = parse_legacy_borrow_notes(&notes) else {
            stats.unparsed += 1;
            continue;
        };

        let stmt = match due {
            Some(due) => Statement::from_sql_and_values(
                backend,
                "UPDATE copies SET lender_display_name = ?, borrow_due_date = ?, borrow_source = ? \
                 WHERE uuid = ?",
                [
                    sea_orm::Value::from(name),
                    sea_orm::Value::from(due),
                    sea_orm::Value::from(source.to_string()),
                    sea_orm::Value::from(id),
                ],
            ),
            None => Statement::from_sql_and_values(
                backend,
                "UPDATE copies SET lender_display_name = ?, borrow_source = ? WHERE uuid = ?",
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

    // --- Migration 078: stable UUIDs ---

    // After the full migration chain (including `migrate_uuid_pk`), every
    // replicated entity carries its stable `uuid` as the PRIMARY KEY. The
    // migration-078 era added `uuid` as a plain column plus a separate
    // `idx_<table>_uuid` UNIQUE index; the uuid-PK rebuild promotes `uuid` to the
    // PK and drops that now-redundant index, which is also required because
    // cr-sqlite forbids a non-PK UNIQUE index on a CRR.
    #[tokio::test]
    async fn replicated_entities_have_uuid_primary_key_and_no_secondary_unique_index() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        for table in ["books", "copies", "authors", "contacts", "tags", "loans"] {
            let cols = db
                .query_all(Statement::from_string(
                    db.get_database_backend(),
                    format!("PRAGMA table_info({table})"),
                ))
                .await
                .expect("table_info");
            let uuid_is_pk = cols.iter().any(|r| {
                let name = r.try_get::<String>("", "name").unwrap_or_default();
                let pk = r.try_get::<i32>("", "pk").unwrap_or(0);
                name == "uuid" && pk > 0
            });
            assert!(
                uuid_is_pk,
                "table {table} must have uuid as its primary key"
            );

            let idx = db
                .query_all(Statement::from_string(
                    db.get_database_backend(),
                    format!("PRAGMA index_list({table})"),
                ))
                .await
                .expect("index_list");
            let has_legacy_idx = idx.iter().any(|r| {
                r.try_get::<String>("", "name")
                    .map(|n| n == format!("idx_{table}_uuid"))
                    .unwrap_or(false)
            });
            assert!(
                !has_legacy_idx,
                "table {table} must NOT keep the secondary idx_{table}_uuid (CRRs forbid it)"
            );
        }
    }

    // The CRR default synthesis: cr-sqlite requires every NOT NULL non-PK column
    // of a CRR to carry a DEFAULT. `crr_default_clause` preserves an existing
    // default, synthesizes a type-appropriate placeholder for NOT NULL non-PK
    // columns that lack one, and leaves PK / nullable columns untouched.
    #[test]
    fn crr_default_clause_synthesizes_only_where_needed() {
        let none: Option<String> = None;

        // An existing default is preserved verbatim (even on a PK / nullable col).
        assert_eq!(
            crr_default_clause(&Some("'to_read'".to_owned()), true, false, "TEXT"),
            " DEFAULT 'to_read'"
        );
        assert_eq!(
            crr_default_clause(&Some("1".to_owned()), true, true, "INTEGER"),
            " DEFAULT 1"
        );

        // NOT NULL, non-PK, no default -> a type-appropriate placeholder.
        assert_eq!(
            crr_default_clause(&none, true, false, "TEXT"),
            " DEFAULT ''"
        );
        assert_eq!(crr_default_clause(&none, true, false, ""), " DEFAULT ''");
        assert_eq!(
            crr_default_clause(&none, true, false, "INTEGER"),
            " DEFAULT 0"
        );
        assert_eq!(crr_default_clause(&none, true, false, "REAL"), " DEFAULT 0");
        assert_eq!(
            crr_default_clause(&none, true, false, "BLOB"),
            " DEFAULT x''"
        );

        // PK columns and nullable columns get no synthesized default.
        assert_eq!(crr_default_clause(&none, true, true, "TEXT"), "");
        assert_eq!(crr_default_clause(&none, false, false, "TEXT"), "");
    }

    #[tokio::test]
    async fn backfill_fills_pre_existing_null_uuid_rows() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        // `backfill_uuids` repairs rows whose `uuid` is NULL: the transient state a
        // table is in after migration 078 adds the column but before each row's
        // value is assigned. Post-082 the live entity tables make `uuid` the NOT
        // NULL primary key, so that NULL state can no longer exist on them; exercise
        // the generic helper against a throwaway table with a nullable `uuid`.
        for stmt in [
            "CREATE TABLE bf_probe (id INTEGER PRIMARY KEY AUTOINCREMENT, uuid TEXT)",
            "INSERT INTO bf_probe (uuid) VALUES (NULL), (NULL), (NULL)",
        ] {
            db.execute(Statement::from_string(
                db.get_database_backend(),
                stmt.to_owned(),
            ))
            .await
            .expect("seed nullable-uuid rows");
        }

        backfill_uuids(&db, "bf_probe").await.expect("backfill");

        let rows = db
            .query_all(Statement::from_string(
                db.get_database_backend(),
                "SELECT uuid FROM bf_probe ORDER BY id".to_owned(),
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

        use sea_orm::{ColumnTrait, QueryFilter};

        let db = init_db("sqlite::memory:").await.expect("init db");
        author::Entity::insert(author::ActiveModel {
            id: NotSet,
            name: Set("Trigger Test".to_owned()),
            created_at: Set("2020".to_owned()),
            updated_at: Set("2020".to_owned()),
        })
        .exec(&db)
        .await
        .expect("insert via Entity::insert");

        // The uuid PK is minted by the AFTER INSERT trigger (before_save is
        // bypassed on the Entity::insert path), so fetch by name rather than id.
        let row = author::Entity::find()
            .filter(author::Column::Name.eq("Trigger Test"))
            .one(&db)
            .await
            .expect("find")
            .expect("row exists");
        assert!(
            !row.id.is_empty(),
            "trigger must set uuid on the Entity::insert path"
        );
        assert_eq!(
            uuid::Uuid::parse_str(&row.id)
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

    // --- Account-sync boot-mode detection (`detect_sync_mode`) ---

    // A migrated but un-enrolled database (no CRR clock tables, no account_session
    // row) is NOT in sync mode, so the real bootstrap opens a plain pool.
    #[cfg(feature = "account_sync")]
    #[tokio::test]
    async fn detect_sync_mode_is_false_for_an_unenrolled_db() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        assert!(!detect_sync_mode(&db).await.expect("detect_sync_mode"));
    }

    // Enrollment persists an account_session row but defers the first setup_crrs to
    // the next boot; that pending row alone must flip the next boot into sync mode.
    #[cfg(feature = "account_sync")]
    #[tokio::test]
    async fn detect_sync_mode_is_true_once_an_account_session_exists() {
        let db = init_db("sqlite::memory:").await.expect("init db");
        db.execute(Statement::from_string(
            db.get_database_backend(),
            "INSERT INTO account_session (id, account_id, email, device_id, encrypted_bundle, salt) \
             VALUES (0, 'acct-1', 'r@e.org', 'dev-1', x'00', x'00')"
                .to_owned(),
        ))
        .await
        .expect("insert account_session");
        assert!(detect_sync_mode(&db).await.expect("detect_sync_mode"));
    }
}
