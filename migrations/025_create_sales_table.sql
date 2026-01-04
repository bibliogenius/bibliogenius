-- Create sales table for bookseller profile
-- Similar structure to loans table but for sales transactions

CREATE TABLE IF NOT EXISTS sales (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    copy_id INTEGER NOT NULL,
    contact_id INTEGER,  -- Client is optional (can sell without tracking customer)
    library_id INTEGER NOT NULL,
    sale_date TEXT NOT NULL,
    sale_price REAL NOT NULL,  -- Actual sale price in EUR
    status TEXT NOT NULL DEFAULT 'completed',  -- 'completed', 'cancelled'
    notes TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (copy_id) REFERENCES copies(id) ON DELETE CASCADE,
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE SET NULL,
    FOREIGN KEY (library_id) REFERENCES libraries(id) ON DELETE CASCADE
);

-- Indexes for performance
CREATE INDEX IF NOT EXISTS idx_sales_copy_id ON sales(copy_id);
CREATE INDEX IF NOT EXISTS idx_sales_contact_id ON sales(contact_id);
CREATE INDEX IF NOT EXISTS idx_sales_library_id ON sales(library_id);
CREATE INDEX IF NOT EXISTS idx_sales_status ON sales(status);
CREATE INDEX IF NOT EXISTS idx_sales_date ON sales(sale_date);
CREATE INDEX IF NOT EXISTS idx_sales_created_at ON sales(created_at);
