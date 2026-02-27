//! mDNS Service for Local Discovery
//!
//! This module provides mDNS-SD (Multicast DNS Service Discovery) functionality
//! to automatically discover BiblioGenius libraries on the local network.
//!
//! Features:
//! - Announce own library on the network
//! - Discover other libraries on the same WiFi
//! - Thread-safe management of discovered peers

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

/// Service type for BiblioGenius mDNS announcements
const SERVICE_TYPE: &str = "_bibliogenius._tcp.local.";

/// Maximum number of discovered peers to keep in memory.
/// On noisy networks (schools, libraries), mDNS can discover many services.
/// Oldest peers are evicted when this limit is exceeded.
pub const MAX_DISCOVERED_PEERS: usize = 50;

/// Represents a discovered peer on the local network
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredPeer {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub addresses: Vec<String>,
    pub library_id: Option<String>,
    /// Ed25519 public key (hex-encoded) from mDNS TXT record
    pub ed25519_public_key: Option<String>,
    /// X25519 public key (hex-encoded) from mDNS TXT record
    pub x25519_public_key: Option<String>,
    pub discovered_at: String,
}

/// Stored configuration for restarting mDNS without requiring callers to re-supply parameters.
#[derive(Clone)]
struct MdnsConfig {
    library_name: String,
    port: u16,
    library_id: Option<String>,
    ed25519_public_key: Option<String>,
    x25519_public_key: Option<String>,
}

/// Evict the oldest peer from the map if it exceeds `max_peers`.
/// Returns the evicted key if one was removed.
fn evict_oldest_peer_if_over_limit(
    peers: &mut HashMap<String, DiscoveredPeer>,
    max_peers: usize,
) -> Option<String> {
    if peers.len() <= max_peers {
        return None;
    }
    let oldest_key = peers
        .iter()
        .min_by_key(|(_, p)| &p.discovered_at)
        .map(|(k, _)| k.clone());
    if let Some(ref key) = oldest_key {
        peers.remove(key);
    }
    oldest_key
}

/// Manages mDNS service announcement and discovery
pub struct MdnsService {
    daemon: ServiceDaemon,
    service_fullname: Option<String>,
    discovered_peers: Arc<RwLock<HashMap<String, DiscoveredPeer>>>,
    is_running: Arc<RwLock<bool>>,
}

impl MdnsService {
    /// Create and start a new mDNS service
    ///
    /// # Arguments
    /// * `library_name` - The name to announce on the network
    /// * `port` - The port the Axum server is listening on
    /// * `library_id` - Optional unique identifier for the library
    /// * `ed25519_public_key` - Optional hex-encoded Ed25519 public key for E2EE
    /// * `x25519_public_key` - Optional hex-encoded X25519 public key for E2EE
    pub fn new(
        library_name: &str,
        port: u16,
        library_id: Option<String>,
        ed25519_public_key: Option<String>,
        x25519_public_key: Option<String>,
    ) -> Result<Self, String> {
        let daemon =
            ServiceDaemon::new().map_err(|e| format!("Failed to create mDNS daemon: {}", e))?;

        let discovered_peers = Arc::new(RwLock::new(HashMap::new()));
        let is_running = Arc::new(RwLock::new(true));

        let mut service = Self {
            daemon,
            service_fullname: None,
            discovered_peers,
            is_running,
        };

        // Register our service
        service.register_service(
            library_name,
            port,
            library_id,
            ed25519_public_key,
            x25519_public_key,
        )?;

        // Start discovery in background
        service.start_discovery()?;

        Ok(service)
    }

    /// Register this library as an mDNS service
    fn register_service(
        &mut self,
        library_name: &str,
        port: u16,
        library_id: Option<String>,
        ed25519_public_key: Option<String>,
        x25519_public_key: Option<String>,
    ) -> Result<(), String> {
        // Sanitize the library name for mDNS (alphanumeric and hyphens only)
        let safe_name: String = library_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == ' ' {
                    c
                } else {
                    '-'
                }
            })
            .collect();

        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "bibliogenius".to_string());

        // Build properties — 64-char hex keys fit well within mDNS TXT limit (~1300 bytes)
        let mut properties = vec![("version", "1.0")];

        let lib_id_string;
        if let Some(ref id) = library_id {
            lib_id_string = id.clone();
            properties.push(("library_id", &lib_id_string));
        }

        let ed_key_string;
        if let Some(ref key) = ed25519_public_key {
            ed_key_string = key.clone();
            properties.push(("ed25519", &ed_key_string));
        }

        let x_key_string;
        if let Some(ref key) = x25519_public_key {
            x_key_string = key.clone();
            properties.push(("x25519", &x_key_string));
        }

        let service_info = ServiceInfo::new(
            SERVICE_TYPE,
            &safe_name,
            &format!("{}.local.", hostname),
            (), // Use all available addresses
            port,
            &properties[..],
        )
        .map_err(|e| format!("Failed to create service info: {}", e))?;

        self.service_fullname = Some(service_info.get_fullname().to_string());

        self.daemon
            .register(service_info)
            .map_err(|e| format!("Failed to register mDNS service: {}", e))?;

        tracing::info!(
            "mDNS: Announcing library '{}' on port {} (e2ee={})",
            safe_name,
            port,
            ed25519_public_key.is_some()
        );

        Ok(())
    }

    /// Start discovering other BiblioGenius libraries on the network
    fn start_discovery(&self) -> Result<(), String> {
        let receiver = self
            .daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| format!("Failed to start mDNS browse: {}", e))?;

        let peers = Arc::clone(&self.discovered_peers);
        let is_running = Arc::clone(&self.is_running);
        let own_fullname = self.service_fullname.clone();

        // Spawn discovery thread
        std::thread::spawn(move || {
            tracing::info!("🔍 mDNS: Starting local network discovery...");

            while *is_running.read().unwrap() {
                match receiver.recv_timeout(Duration::from_secs(1)) {
                    Ok(event) => {
                        match event {
                            ServiceEvent::ServiceResolved(info) => {
                                let fullname = info.get_fullname().to_string();

                                // Skip our own service
                                if Some(&fullname) == own_fullname.as_ref() {
                                    continue;
                                }

                                let peer = DiscoveredPeer {
                                    name: info
                                        .get_hostname()
                                        .trim_end_matches(".local.")
                                        .to_string(),
                                    host: info.get_hostname().to_string(),
                                    port: info.get_port(),
                                    addresses: info
                                        .get_addresses()
                                        .iter()
                                        .map(|a| a.to_string())
                                        .collect(),
                                    library_id: info
                                        .get_property_val_str("library_id")
                                        .map(|s| s.to_string()),
                                    ed25519_public_key: info
                                        .get_property_val_str("ed25519")
                                        .map(|s| s.to_string()),
                                    x25519_public_key: info
                                        .get_property_val_str("x25519")
                                        .map(|s| s.to_string()),
                                    discovered_at: chrono::Utc::now().to_rfc3339(),
                                };

                                tracing::info!(
                                    "📚 mDNS: Discovered library '{}' at {}:{}",
                                    peer.name,
                                    peer.addresses.first().unwrap_or(&"?".to_string()),
                                    peer.port
                                );

                                let mut peers_guard = peers.write().unwrap();
                                peers_guard.insert(fullname, peer);

                                if evict_oldest_peer_if_over_limit(
                                    &mut peers_guard,
                                    MAX_DISCOVERED_PEERS,
                                )
                                .is_some()
                                {
                                    tracing::warn!(
                                        "mDNS: Peer limit ({}) reached, evicted oldest entry",
                                        MAX_DISCOVERED_PEERS
                                    );
                                }
                                drop(peers_guard);
                            }
                            ServiceEvent::ServiceRemoved(_, fullname) => {
                                if let Some(peer) = peers.write().unwrap().remove(&fullname) {
                                    tracing::info!(
                                        "👋 mDNS: Library '{}' left the network",
                                        peer.name
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        // Check error message to determine if it's a timeout or disconnection
                        let err_msg = format!("{:?}", e);
                        if err_msg.contains("Disconnected") {
                            tracing::warn!("mDNS browse channel disconnected");
                            break;
                        }
                        // Timeout or other error, continue loop
                    }
                }
            }

            tracing::info!("🔍 mDNS: Discovery stopped");
        });

        Ok(())
    }

    /// Get list of currently discovered peers
    pub fn get_discovered_peers(&self) -> Vec<DiscoveredPeer> {
        self.discovered_peers
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    /// Check if the mDNS service is running
    pub fn is_running(&self) -> bool {
        *self.is_running.read().unwrap()
    }

    /// Stop the mDNS service
    pub fn stop(&self) {
        *self.is_running.write().unwrap() = false;

        // Unregister our service
        if let Some(ref fullname) = self.service_fullname {
            let _ = self.daemon.unregister(fullname);
        }

        let _ = self.daemon.shutdown();
        tracing::info!("📡 mDNS: Service stopped");
    }
}

impl Drop for MdnsService {
    fn drop(&mut self) {
        self.stop();
    }
}

// Global singleton for the mDNS service
static MDNS_SERVICE: OnceLock<RwLock<Option<MdnsService>>> = OnceLock::new();

// Stored config so we can restart mDNS without re-supplying parameters
static MDNS_CONFIG: OnceLock<RwLock<Option<MdnsConfig>>> = OnceLock::new();

/// Initialize the global mDNS service
pub fn init_mdns(
    library_name: &str,
    port: u16,
    library_id: Option<String>,
    ed25519_public_key: Option<String>,
    x25519_public_key: Option<String>,
) -> Result<(), String> {
    // Save config for potential restarts
    let config = MdnsConfig {
        library_name: library_name.to_string(),
        port,
        library_id: library_id.clone(),
        ed25519_public_key: ed25519_public_key.clone(),
        x25519_public_key: x25519_public_key.clone(),
    };
    let config_global = MDNS_CONFIG.get_or_init(|| RwLock::new(None));
    *config_global.write().unwrap() = Some(config);

    let service = MdnsService::new(
        library_name,
        port,
        library_id,
        ed25519_public_key,
        x25519_public_key,
    )?;

    let global = MDNS_SERVICE.get_or_init(|| RwLock::new(None));
    *global.write().unwrap() = Some(service);

    Ok(())
}

/// Restart mDNS using the previously stored configuration.
/// Returns an error if init_mdns was never called.
pub fn restart_mdns() -> Result<(), String> {
    let config = MDNS_CONFIG
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|opt| opt.clone())
        .ok_or_else(|| {
            "Cannot restart mDNS: no stored configuration (init_mdns was never called)".to_string()
        })?;

    stop_mdns();

    init_mdns(
        &config.library_name,
        config.port,
        config.library_id,
        config.ed25519_public_key,
        config.x25519_public_key,
    )
}

/// Get discovered peers from the global service
pub fn get_local_peers() -> Vec<DiscoveredPeer> {
    MDNS_SERVICE
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|opt| opt.as_ref().map(|s| s.get_discovered_peers()))
        .unwrap_or_default()
}

/// Get the number of currently discovered peers
pub fn get_local_peer_count() -> usize {
    MDNS_SERVICE
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|opt| {
            opt.as_ref()
                .map(|s| s.discovered_peers.read().unwrap().len())
        })
        .unwrap_or(0)
}

/// Check if mDNS is currently active
pub fn is_mdns_active() -> bool {
    MDNS_SERVICE
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|opt| opt.is_some())
        .unwrap_or(false)
}

/// Stop the global mDNS service
pub fn stop_mdns() {
    if let Some(global) = MDNS_SERVICE.get()
        && let Ok(mut lock) = global.write()
        && let Some(service) = lock.take()
    {
        service.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a DiscoveredPeer with a given timestamp string.
    fn make_peer(name: &str, discovered_at: &str) -> DiscoveredPeer {
        DiscoveredPeer {
            name: name.to_string(),
            host: format!("{}.local.", name),
            port: 8000,
            addresses: vec!["192.168.1.1".to_string()],
            library_id: None,
            ed25519_public_key: None,
            x25519_public_key: None,
            discovered_at: discovered_at.to_string(),
        }
    }

    #[test]
    fn test_service_type_format() {
        assert!(SERVICE_TYPE.starts_with("_"));
        assert!(SERVICE_TYPE.ends_with(".local."));
    }

    #[test]
    fn test_max_discovered_peers_is_reasonable() {
        assert!(MAX_DISCOVERED_PEERS >= 10, "limit too low for normal use");
        assert!(
            MAX_DISCOVERED_PEERS <= 200,
            "limit too high, defeats purpose"
        );
    }

    #[test]
    fn test_eviction_does_nothing_under_limit() {
        let mut peers = HashMap::new();
        peers.insert("a".to_string(), make_peer("a", "2026-01-01T00:00:00Z"));
        peers.insert("b".to_string(), make_peer("b", "2026-01-02T00:00:00Z"));

        let evicted = evict_oldest_peer_if_over_limit(&mut peers, 5);
        assert!(evicted.is_none());
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn test_eviction_does_nothing_at_exact_limit() {
        let mut peers = HashMap::new();
        for i in 0..3 {
            peers.insert(
                format!("peer-{i}"),
                make_peer(
                    &format!("lib-{i}"),
                    &format!("2026-01-0{}T00:00:00Z", i + 1),
                ),
            );
        }

        let evicted = evict_oldest_peer_if_over_limit(&mut peers, 3);
        assert!(evicted.is_none());
        assert_eq!(peers.len(), 3);
    }

    #[test]
    fn test_eviction_removes_oldest_peer() {
        let mut peers = HashMap::new();
        peers.insert(
            "old".to_string(),
            make_peer("oldest-lib", "2026-01-01T00:00:00Z"),
        );
        peers.insert(
            "mid".to_string(),
            make_peer("middle-lib", "2026-01-02T00:00:00Z"),
        );
        peers.insert(
            "new".to_string(),
            make_peer("newest-lib", "2026-01-03T00:00:00Z"),
        );

        // limit=2 but we have 3 -> evict oldest
        let evicted = evict_oldest_peer_if_over_limit(&mut peers, 2);
        assert_eq!(evicted, Some("old".to_string()));
        assert_eq!(peers.len(), 2);
        assert!(!peers.contains_key("old"));
        assert!(peers.contains_key("mid"));
        assert!(peers.contains_key("new"));
    }

    #[test]
    fn test_eviction_at_max_discovered_peers_limit() {
        let mut peers = HashMap::new();
        // Fill to MAX + 1
        for i in 0..=MAX_DISCOVERED_PEERS {
            peers.insert(
                format!("peer-{i:03}"),
                make_peer(
                    &format!("lib-{i}"),
                    &format!("2026-01-01T{:02}:00:00Z", i % 24),
                ),
            );
        }
        assert_eq!(peers.len(), MAX_DISCOVERED_PEERS + 1);

        let evicted = evict_oldest_peer_if_over_limit(&mut peers, MAX_DISCOVERED_PEERS);
        assert!(evicted.is_some());
        assert_eq!(peers.len(), MAX_DISCOVERED_PEERS);
    }

    #[test]
    fn test_eviction_repeated_stays_bounded() {
        let mut peers = HashMap::new();
        // Simulate adding peers one by one, evicting after each
        for i in 0..100 {
            peers.insert(
                format!("peer-{i}"),
                make_peer(
                    &format!("lib-{i}"),
                    &format!("2026-01-01T00:{:02}:00Z", i % 60),
                ),
            );
            evict_oldest_peer_if_over_limit(&mut peers, 5);
        }
        assert!(peers.len() <= 5);
    }

    #[test]
    fn test_mdns_config_clone_preserves_fields() {
        let config = MdnsConfig {
            library_name: "My Library".to_string(),
            port: 8080,
            library_id: Some("lib-123".to_string()),
            ed25519_public_key: Some("aabbcc".to_string()),
            x25519_public_key: None,
        };
        let cloned = config.clone();
        assert_eq!(cloned.library_name, "My Library");
        assert_eq!(cloned.port, 8080);
        assert_eq!(cloned.library_id, Some("lib-123".to_string()));
        assert_eq!(cloned.ed25519_public_key, Some("aabbcc".to_string()));
        assert!(cloned.x25519_public_key.is_none());
    }

    #[test]
    fn test_discovered_peer_serialization_roundtrip() {
        let peer = make_peer("test-lib", "2026-02-27T10:00:00Z");
        let json = serde_json::to_string(&peer).expect("serialize");
        let deserialized: DiscoveredPeer = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.name, "test-lib");
        assert_eq!(deserialized.discovered_at, "2026-02-27T10:00:00Z");
        assert_eq!(deserialized.port, 8000);
    }
}
