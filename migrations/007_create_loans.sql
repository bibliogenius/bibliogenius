CREATE TABLE IF NOT EXISTS loans (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    copy_id INTEGER NOT NULL,
    contact_id INTEGER NOT NULL,
    library_id INTEGER NOT NULL,
    loan_date TEXT NOT NULL,
    due_date TEXT NOT NULL,
    return_date TEXT,
    status TEXT NOT NULL DEFAULT 'active', -- 'active', 'returned', 'overdue', 'lost'
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
