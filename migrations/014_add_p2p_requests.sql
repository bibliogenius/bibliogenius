CREATE TABLE p2p_requests (
    id TEXT PRIMARY KEY,
    from_peer_id INTEGER NOT NULL,
    book_isbn TEXT NOT NULL,
    book_title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (from_peer_id) REFERENCES peers(id)
);
