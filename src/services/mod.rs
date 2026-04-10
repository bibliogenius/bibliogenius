//! Services Layer
//!
//! This module contains pure business logic extracted from HTTP handlers.
//! Services can be called directly via FFI or through Axum handlers.

pub mod book_service;
pub mod catalog_events;
pub mod catalog_notification;
pub mod contact_service;
pub mod crypto_service;
pub mod device_pairing_service;
pub mod device_sync_service;
pub mod e2ee_transport;
pub mod gamification_service;
pub mod hub_directory_service;
pub mod identity_service;
pub mod loan_service;
pub mod lookup_service;
pub mod mdns;
pub mod notification_service;
pub mod nudge_events;
pub mod relay_poller;
pub mod relay_transport;
pub mod sale_service; // Service de vente pour profil Libraire
pub mod ws_nudge;

// Re-export for convenience
pub use book_service::*;
pub use identity_service::IdentityService;
pub use mdns::{
    DiscoveredPeer, MAX_DISCOVERED_PEERS, get_local_peer_count, get_local_peers, init_mdns,
    is_mdns_active, restart_mdns, stop_mdns,
};
