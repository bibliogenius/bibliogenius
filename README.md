# BiblioGenius - Rust Server

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Version](https://img.shields.io/badge/version-0.7.0--alpha-blue)](https://github.com/bibliogenius/bibliogenius/releases)

**Autonomous library server with REST API, P2P synchronization, and local storage.**

This is the core backend for the BiblioGenius ecosystem, written in Rust. It powers the mobile/desktop app via FFI or runs as a standalone server.

## Features

- **High Performance**: Built with Rust, Axum, and Tokio.
- **Dual Mode**: Runs embedded (FFI) or standalone (HTTP).
- **Local First**: Full offline capabilities with SQLite (via SeaORM).
- **P2P Sync**: Decentralized library sharing between instances.
- **MCP Server**: Optional Model Context Protocol mode for AI integration.
- **External Sources**: Metadata lookup from BNF, Inventaire, OpenLibrary, Google Books.

## Prerequisites

- **Rust**: Latest stable toolchain (`rustup update stable`)
- **SQLite**: `libsqlite3-dev` (Linux) or bundled (macOS/Windows)
- **Dart/Flutter**: Only if developing the frontend app

## Quick Start

### Standalone Server

Run the server independently for testing or headless deployment:

```bash
# Clone repository
git clone https://github.com/bibliogenius/bibliogenius.git
cd bibliogenius

# Run server
cargo run
```

The API will be available at `http://localhost:8000`. Swagger UI is served at `/api/docs`.

### Embedded Mode (FFI)

If you are developing the [Flutter App](https://github.com/bibliogenius/bibliogenius-app), you generally **do not** need to run this repo manually. The `bibliogenius-app` build process (via Cargokit) automatically compiles and links this Rust crate.

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

FFI bindings for the Flutter app are defined in `api/frb.rs` via [Flutter Rust Bridge](https://github.com/aspect-build/rules_lint).

## Development

```bash
# Build
cargo build

# Run all checks (format + lint + tests)
cargo fmt && cargo clippy -- -D warnings && cargo test
```

## Related Repositories

- [**bibliogenius-app**](https://github.com/bibliogenius/bibliogenius-app): Flutter frontend (mobile & desktop).

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.