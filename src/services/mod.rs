//! Services Layer
//!
//! This module contains pure business logic extracted from HTTP handlers.
//! Services can be called directly via FFI or through Axum handlers.

pub mod book_service;
pub mod contact_service;
pub mod crypto_service;
pub mod e2ee_transport;
pub mod identity_service;
pub mod loan_service;
pub mod lookup_service;
pub mod mdns;
pub mod relay_poller;
pub mod relay_transport;
pub mod sale_service; // Service de vente pour profil Libraire

// Re-export for convenience
pub use book_service::*;
pub use identity_service::IdentityService;
pub use mdns::{DiscoveredPeer, get_local_peers, init_mdns, is_mdns_active, stop_mdns};
