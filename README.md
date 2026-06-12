# BiblioGenius - Rust Server

> **Canonical repository: [Codeberg](https://codeberg.org/bibliogenius/bibliogenius).** The GitHub copy is a read-only mirror, automatically synced from Codeberg. Please open issues and pull requests on Codeberg.

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Version](https://img.shields.io/gitea/v/tag/bibliogenius/bibliogenius?gitea_url=https%3A%2F%2Fcodeberg.org&sort=semver&label=version)](https://codeberg.org/bibliogenius/bibliogenius/tags)

**Autonomous library server with REST API, P2P synchronization, and local storage.**

This is the core backend of the BiblioGenius ecosystem, written in Rust. It owns the library database, the synchronization logic, and external metadata lookups. It runs in two modes from the same codebase:

- **Embedded (FFI)**: compiled into the [Flutter app](https://codeberg.org/bibliogenius/bibliogenius-app) for offline-first performance on iOS, Android, and macOS (and the Windows/Linux desktop builds). This is how most users run it, without ever knowing it is there.
- **Standalone (HTTP)**: run on its own as a headless server for testing, self-hosting, or scripting against the REST API.

## Features

- **High Performance**: Built with Rust, Axum, and Tokio.
- **Dual Mode**: Runs embedded (FFI) or standalone (HTTP) from one codebase.
- **Local First**: Full offline capabilities with SQLite (via SeaORM).
- **P2P Sync**: Decentralized library sharing between instances, over LAN (mDNS) or an E2EE relay hub.
- **MCP Server**: Optional Model Context Protocol mode, so local AI agents can query your library.
- **External Sources**: Metadata lookup from BNF, Inventaire, OpenLibrary, and Google Books.

## Prerequisites

- **Rust**: Latest stable toolchain (`rustup update stable`)
- **SQLite**: `libsqlite3-dev` (Linux) or bundled (macOS/Windows)
- **Dart/Flutter**: Only if developing the frontend app

## Quick Start

### Standalone Server

Run the server independently for testing or headless deployment:

```bash
# Clone repository
git clone https://codeberg.org/bibliogenius/bibliogenius.git
cd bibliogenius

# Run server
cargo run
```

The API is available at `http://localhost:8000`. Swagger UI is served at `/api/docs`.

### Embedded Mode (FFI)

If you are developing the [Flutter App](https://codeberg.org/bibliogenius/bibliogenius-app), you generally **do not** need to run this repo manually. The `bibliogenius-app` build process (via Cargokit) automatically compiles and links this Rust crate.

## Architecture

The backend follows a **Clean Architecture** (migration in progress):

```
src/
├── api/              # HTTP handlers (Axum) — thin delegation layer
├── domain/           # Pure business abstractions (traits, errors — no framework deps)
├── services/         # Business logic orchestration
├── infrastructure/   # SeaORM repository implementations, config, auth
├── models/           # DTOs and SeaORM entities (API contract)
└── modules/          # External integrations (BNF, Inventaire, OpenLibrary…)
```

FFI bindings for the Flutter app are defined in `api/frb.rs` via [flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge).

## Development

```bash
# Build
cargo build

# Run all checks (format + lint + tests)
cargo fmt && cargo clippy -- -D warnings && cargo test
```

## Related Repositories

- [**bibliogenius-app**](https://codeberg.org/bibliogenius/bibliogenius-app): Flutter frontend (mobile & desktop) that embeds this server.
- [**bibliogenius-hub**](https://codeberg.org/bibliogenius/bibliogenius-hub): Optional E2EE relay hub for off-network sync.

## License

This project is licensed under the GNU Affero General Public License v3.0 - see the [LICENSE](LICENSE) file for details.
