// FFI API module for flutter_rust_bridge
// This module exposes core functionality to Flutter without HTTP layer
//
// ARCHITECTURE: This module provides direct database access for all native platforms.
// Web uses WASM (future). All native platforms use FFI for local-first operation.

use flutter_rust_bridge::frb;
use sea_orm::{ActiveModelTrait, DatabaseConnection};
use std::sync::OnceLock;
use tokio::runtime::Runtime;
use tower_http::cors::{Any, CorsLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// One file per concern, textually included so every item stays in
// crate::api::frb: the flutter_rust_bridge codegen namespaces items by their
// defining module, so real submodules would rename the generated Dart
// bindings. The include! order preserves the original declaration order,
// which the generated Dart facade follows. See the FFI module layout note
// in .agents/instructions/architecture.md before touching this layout.
include!("frb/core.rs");
include!("frb/book_dto.rs");
include!("frb/lifecycle.rs");
include!("frb/mdns.rs");
include!("frb/identity.rs");
include!("frb/uuid_wrappers.rs");
include!("frb/account_sync.rs");
include!("frb/book_conversions.rs");
include!("frb/library_name.rs");
include!("frb/books.rs");
include!("frb/covers.rs");
include!("frb/metadata_fill.rs");
include!("frb/tags.rs");
include!("frb/contacts.rs");
include!("frb/loans.rs");
include!("frb/server_control.rs");
include!("frb/events.rs");
include!("frb/games.rs");
include!("frb/gamification.rs");
include!("frb/search_settings.rs");
include!("frb/operation_log.rs");
include!("frb/hub_directory.rs");
include!("frb/hub_catalog.rs");
include!("frb/hub_follows.rs");
include!("frb/collections.rs");
include!("frb/notifications.rs");
include!("frb/book_notes.rs");
include!("frb/backup.rs");
include!("frb/hub_catalog_tests.rs");
