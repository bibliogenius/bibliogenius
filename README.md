# BiblioGenius - Rust Server

Autonomous library server with REST API, P2P synchronization, and local storage.

## 🏗️ How It Works

The Rust backend can run in two modes:

| Mode | Description | Use Case |
| ---- | ----------- | -------- |
| **FFI (Primary)** | Embedded in Flutter app via Cargokit | Normal app usage |
| **Standalone HTTP** | Runs as separate HTTP server | Testing, development, or headless mode |

> **For app development**: You don't need to run `cargo run` manually. The Rust code is automatically compiled when you run `flutter run` (via Cargokit).

## Tech Stack

- **Language**: Rust
- **Framework**: Axum
- **Database**: SQLite + SeaORM
- **Search**: Tantivy
- **Auth**: JWT with local keypair
- **FFI Bridge**: Flutter Rust Bridge

## Features

- REST API for book management
- Local authentication (JWT)
- P2P synchronization with other servers (mDNS discovery)
- Full-text search (Tantivy)
- Export/import capabilities
- Embedded FFI mode for Flutter apps

## Getting Started

### For App Development (Recommended)

No manual Rust commands needed! Simply:

```bash
cd ../bibliogenius-app
flutter pub get
flutter run -d macos
```

The Rust backend compiles automatically via Cargokit.

### For Standalone HTTP Server

```bash
# Build
cargo build

# Run (starts HTTP server on port 8000)
cargo run

# Run tests
cargo test
```

## API Endpoints

```http
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

| Version            | Status     | Focus                                         |
| ------------------ | ---------- | --------------------------------------------- |
| **In Development** | ✅ Current | Personal library + LAN sync                   |
| v1.0.0             | Q1 2026    | Stable P2P on local network                   |
| v1.x               | Q2 2026    | Dynamic AI Bibliographies (Data.BnF + Gemini) |
| v2.0.0             | Q2-Q3 2026 | Global P2P (libp2p) + Social                  |

## Repository

<https://github.com/bibliogenius/bibliogenius>
