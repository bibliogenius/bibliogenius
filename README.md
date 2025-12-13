# BiblioGenius - Rust Server

Autonomous library server with REST API, P2P synchronization, and local storage.

## Tech Stack

- **Language**: Rust
- **Framework**: Axum
- **Database**: SQLite + SeaORM
- **Search**: Tantivy
- **Auth**: JWT with local keypair

## Features

- REST API for book management
- Local authentication
- P2P synchronization with other servers
- Full-text search
- Export/import capabilities

## Getting Started

```bash
# Build
cargo build

# Run
cargo run

# Test
cargo test
```

## API Endpoints

```
GET  /api/health              # Health check
GET  /api/library/config      # Get library info
POST /api/library/config      # Update library info

GET  /api/books               # List books
POST /api/books               # Add book
GET  /api/books/{id}          # Get book
PUT  /api/books/{id}          # Update book
DELETE /api/books/{id}        # Delete book

POST /api/hub/register        # Register with hub
GET  /api/hub/discover        # Discover peers
```

## 🗺️ Roadmap

| Version | Status | Focus |
|---------|--------|-------|
| **v1.0.0-beta** | ✅ Current | Personal library + LAN sync |
| v1.0.0 | Q1 2026 | Stable P2P on local network |
| v2.0.0 | Q2-Q3 2026 | Global P2P (libp2p) + Social |

## Repository

<https://github.com/bibliogenius/bibliogenius>
