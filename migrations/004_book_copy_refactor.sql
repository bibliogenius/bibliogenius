-- Migration 004: Book/Copy Architecture Refactoring
-- Separates bibliographic metadata (books) from physical exemplars (copies)

-- 1. Create libraries table
CREATE TABLE IF NOT EXISTS libraries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  description TEXT,
  owner_id INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (owner_id) REFERENCES users(id)
);

-- 2. Create new books table (metadata only, no author field)
CREATE TABLE IF NOT EXISTS books_new (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  isbn TEXT,
  summary TEXT,
  publisher TEXT,
  publication_year INTEGER,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- 3. Create copies table (physical exemplars)
CREATE TABLE IF NOT EXISTS copies (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  book_id INTEGER NOT NULL,
  library_id INTEGER NOT NULL,
  acquisition_date TEXT,
  notes TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (book_id) REFERENCES books(id) ON DELETE CASCADE,
  FOREIGN KEY (library_id) REFERENCES libraries(id) ON DELETE CASCADE
);

-- 4. Migrate existing books data (excluding author field)
INSERT INTO books_new (id, title, isbn, summary, created_at, updated_at)
SELECT id, title, isbn, summary, created_at, updated_at FROM books;

-- 5. Create default library for admin user (id=1)
INSERT INTO libraries (name, description, owner_id, created_at, updated_at)
VALUES ('My Library', 'Default library', 1, datetime('now'), datetime('now'));

-- 6. Create one copy per existing book in the default library
INSERT INTO copies (book_id, library_id, acquisition_date, created_at, updated_at)
SELECT id, 1, created_at, created_at, updated_at FROM books;

-- 7. Drop old books table and rename new one
DROP TABLE books;
ALTER TABLE books_new RENAME TO books;

-- 8. Update operation_log to track copy operations
-- (operation_log already tracks table_name, so it will work with 'copies' table)
