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
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#.to_owned(),
    )).await?;

    // Create library_config table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS library_config (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            description TEXT,
            tags TEXT NOT NULL DEFAULT '[]',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#.to_owned(),
    )).await?;

    // Insert default library config if not exists
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        INSERT OR IGNORE INTO library_config (id, name, description, tags, created_at, updated_at)
        VALUES (1, 'My Library', 'Personal book collection', '[]', datetime('now'), datetime('now'))
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

    // Create peers table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS peers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            public_key TEXT,
            last_seen TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

    // Migration 005: Add status and is_temporary columns if they don't exist
    // Note: SQLite doesn't support IF NOT EXISTS in ALTER TABLE, so we ignore errors
    let _ = db.execute(Statement::from_string(
        db.get_database_backend(),
        "ALTER TABLE copies ADD COLUMN status TEXT NOT NULL DEFAULT 'available'".to_owned(),
    )).await;

    let _ = db.execute(Statement::from_string(
        db.get_database_backend(),
        "ALTER TABLE copies ADD COLUMN is_temporary INTEGER NOT NULL DEFAULT 0".to_owned(),
    )).await;

    // Create indexes for copies
    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_copies_status ON copies(status)".to_owned(),
    )).await?;

    db.execute(Statement::from_string(
        db.get_database_backend(),
        "CREATE INDEX IF NOT EXISTS idx_copies_temporary ON copies(is_temporary)".to_owned(),
    )).await?;

    // Create contacts table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS contacts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            type TEXT NOT NULL,
            name TEXT NOT NULL,
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
        "#.to_owned(),
    )).await?;

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
        "#.to_owned(),
    )).await?;

    // Create peers table
    db.execute(Statement::from_string(
        db.get_database_backend(),
        r#"
        CREATE TABLE IF NOT EXISTS peers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            public_key TEXT,
            last_seen TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_peers_url ON peers(url);
        "#.to_owned(),
    )).await?;

    Ok(())
}
