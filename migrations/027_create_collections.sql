-- Create collections table
CREATE TABLE IF NOT EXISTS collections (
    id TEXT PRIMARY KEY NOT NULL, -- UUID
    name TEXT NOT NULL,
    description TEXT,
    source TEXT NOT NULL, -- 'manual', 'csv_import', 'open_library', etc.
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Create collection_books table
CREATE TABLE IF NOT EXISTS collection_books (
    id TEXT PRIMARY KEY NOT NULL, -- UUID
    collection_id TEXT NOT NULL,
    isbn TEXT,
    title TEXT NOT NULL,
    author TEXT,
    status TEXT NOT NULL DEFAULT 'wanted', -- 'owned', 'wanted', 'ignored'
    cover_url TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (collection_id) REFERENCES collections(id) ON DELETE CASCADE
);

-- Index for searching books in collections
CREATE INDEX IF NOT EXISTS idx_collection_books_collection_id ON collection_books(collection_id);
CREATE INDEX IF NOT EXISTS idx_collection_books_isbn ON collection_books(isbn);
CREATE INDEX IF NOT EXISTS idx_collection_books_status ON collection_books(status);
