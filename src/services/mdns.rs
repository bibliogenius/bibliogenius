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
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Service type for BiblioGenius mDNS announcements
const SERVICE_TYPE: &str = "_bibliogenius._tcp.local.";

/// Represents a discovered peer on the local network
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredPeer {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub addresses: Vec<String>,
    pub library_id: Option<String>,
    pub discovered_at: String,
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
    pub fn new(library_name: &str, port: u16, library_id: Option<String>) -> Result<Self, String> {
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
        service.register_service(library_name, port, library_id)?;

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

        // Build properties
        let mut properties = vec![("version", "1.0")];

        let lib_id_string;
        if let Some(ref id) = library_id {
            lib_id_string = id.clone();
            properties.push(("library_id", &lib_id_string));
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
            "📡 mDNS: Announcing library '{}' on port {}",
            safe_name,
            port
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
                                    discovered_at: chrono::Utc::now().to_rfc3339(),
                                };

                                tracing::info!(
                                    "📚 mDNS: Discovered library '{}' at {}:{}",
                                    peer.name,
                                    peer.addresses.first().unwrap_or(&"?".to_string()),
                                    peer.port
                                );

                                peers.write().unwrap().insert(fullname, peer);
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
static MDNS_SERVICE: std::sync::OnceLock<RwLock<Option<MdnsService>>> = std::sync::OnceLock::new();

/// Initialize the global mDNS service
pub fn init_mdns(library_name: &str, port: u16, library_id: Option<String>) -> Result<(), String> {
    let service = MdnsService::new(library_name, port, library_id)?;

    let global = MDNS_SERVICE.get_or_init(|| RwLock::new(None));
    *global.write().unwrap() = Some(service);

    Ok(())
}

/// Get discovered peers from the global service
pub fn get_local_peers() -> Vec<DiscoveredPeer> {
    MDNS_SERVICE
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|opt| opt.as_ref().map(|s| s.get_discovered_peers()))
        .unwrap_or_default()
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
    if let Some(global) = MDNS_SERVICE.get() {
        if let Ok(mut lock) = global.write() {
            if let Some(service) = lock.take() {
                service.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_type_format() {
        assert!(SERVICE_TYPE.starts_with("_"));
        assert!(SERVICE_TYPE.ends_with(".local."));
    }
}
