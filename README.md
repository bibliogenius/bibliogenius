# BiblioGenius - Rust Server

[![Build Status](https://img.shields.io/badge/build-passing-brightgreen)](https://github.com/bibliogenius/bibliogenius/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Version](https://img.shields.io/badge/version-0.6.5--alpha-blue)](https://github.com/bibliogenius/bibliogenius/releases)

**Autonomous library server with REST API, P2P synchronization, and local storage.**

This is the core backend for the BiblioGenius ecosystem, written in Rust. It powers the mobile/desktop app via FFI or runs as a standalone server.

## 🚀 Features

- **High Performance**: Built with Rust and Actix Web.
- **Dual Mode**: Runs embedded (FFI) or standalone (HTTP).
- **Local First**: Full offline capabilities with SQLite.
- **P2P Sync**: Decentralized library sharing.

## 📋 Prerequisites

- **Rust**: Latest stable toolchain (`rustup update stable`)
- **SQLite**: `libsqlite3-dev` (Linux) or bundled (macOS/Windows)
- **Dart/Flutter**: Only if developing connection layers

## ⚡ Quick Start

### Standalone Server

Run the server independently for testing or headless deployment:

```bash
# Clone repository
git clone https://github.com/bibliogenius/bibliogenius.git
cd bibliogenius

# Run server
cargo run
```

The API will be available at `http://localhost:8000`.

### Embedded Mode (FFI)

If you are developing the [Flutter App](https://github.com/bibliogenius/bibliogenius-app), you generally **do not** need to run this repo manually. The `bibliogenius-app` build process (via Cargokit) automatically compiles and links this Rust crate.

## 🏗️ Architecture

The backend is designed as a modular library:

1. **`src/api`**: REST endpoints (Actix Web).
2. **`src/db`**: Database schema and migrations (Rusqlite).
3. **`src/modules`**: Business logic (Search, Import, P2P).
4. **`src/bridge`**: Flutter Rust Bridge (FRB) definitions for FFI.

## 🛠️ Development Setup

1. **Install dependencies**:

    ```bash
    cargo build
    ```

2. **Run tests**:

    ```bash
    cargo test
    ```

3. **Run clippy (linter)**:

    ```bash
    cargo clippy -- -D warnings
    ```

## 🔗 Related Repositories

- [**bibliogenius-app**](https://github.com/bibliogenius/bibliogenius-app): The frontend Flutter application.
- [**bibliogenius-docker**](https://github.com/bibliogenius/bibliogenius-docker): Docker Compose environment.
- [**bibliogenius-docs**](https://github.com/bibliogenius/bibliogenius-docs): Documentation Hub.

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
