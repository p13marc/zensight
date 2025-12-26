//! Cross-bridge correlation registry.
//!
//! This module provides device identity correlation across protocol bridges.
//! It enables linking data from different sources (SNMP, NetFlow, Syslog, etc.)
//! to the same physical device using IP addresses and hostnames.
//!
//! # Key Expression
//!
//! Correlation data is published to `zensight/_meta/correlation/<ip>`.
//!
//! # Example
//!
//! ```ignore
//! use zensight_bridge_framework::{CorrelationRegistry, DeviceIdentity};
//!
//! let registry = CorrelationRegistry::new(publisher);
//!
//! // Register a device seen by the SNMP bridge
//! registry.register_device(DeviceIdentity {
//!     ip: "10.0.0.1".parse().unwrap(),
//!     hostnames: vec!["router01".to_string()],
//!     bridge: "snmp".to_string(),
//!     source_id: "router01".to_string(),
//! }).await;
//! ```

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::publisher::Publisher;

/// Device identity for correlation purposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Primary IP address.
    pub ip: IpAddr,
    /// Known hostnames for this device.
    pub hostnames: Vec<String>,
    /// Bridge that registered this identity.
    pub bridge: String,
    /// Source identifier used by the bridge.
    pub source_id: String,
}

/// Correlation entry aggregating identities across bridges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationEntry {
    /// Primary IP address.
    pub ip: IpAddr,
    /// All known hostnames across bridges.
    pub hostnames: Vec<String>,
    /// Bridges that have seen this device.
    pub bridges: Vec<String>,
    /// Source IDs per bridge.
    pub sources: HashMap<String, String>,
    /// Last update timestamp.
    pub last_updated: i64,
}

impl CorrelationEntry {
    /// Create a new correlation entry from a device identity.
    fn from_identity(identity: &DeviceIdentity) -> Self {
        let mut sources = HashMap::new();
        sources.insert(identity.bridge.clone(), identity.source_id.clone());

        Self {
            ip: identity.ip,
            hostnames: identity.hostnames.clone(),
            bridges: vec![identity.bridge.clone()],
            sources,
            last_updated: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Merge another identity into this entry.
    fn merge(&mut self, identity: &DeviceIdentity) {
        // Add hostnames (deduplicated)
        for hostname in &identity.hostnames {
            if !self.hostnames.contains(hostname) {
                self.hostnames.push(hostname.clone());
            }
        }

        // Add bridge if not present
        if !self.bridges.contains(&identity.bridge) {
            self.bridges.push(identity.bridge.clone());
        }

        // Update source mapping
        self.sources
            .insert(identity.bridge.clone(), identity.source_id.clone());

        self.last_updated = chrono::Utc::now().timestamp_millis();
    }
}

/// Registry for cross-bridge device correlation.
///
/// Maintains a mapping of IP addresses to device identities and
/// publishes correlation hints to Zenoh for frontend consumption.
#[derive(Debug)]
pub struct CorrelationRegistry {
    /// Correlation entries by IP.
    entries: Arc<RwLock<HashMap<IpAddr, CorrelationEntry>>>,
    /// Hostname to IP lookup.
    hostname_index: Arc<RwLock<HashMap<String, IpAddr>>>,
    /// Publisher for correlation data.
    publisher: Option<Publisher>,
}

impl CorrelationRegistry {
    /// Create a new correlation registry.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            hostname_index: Arc::new(RwLock::new(HashMap::new())),
            publisher: None,
        }
    }

    /// Create a registry with a publisher for Zenoh updates.
    pub fn with_publisher(publisher: Publisher) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            hostname_index: Arc::new(RwLock::new(HashMap::new())),
            publisher: Some(publisher),
        }
    }

    /// Register a device identity.
    ///
    /// If the IP is already known, the entry is merged.
    /// If a hostname is already associated with a different IP, the old association is updated.
    pub async fn register_device(&self, identity: DeviceIdentity) -> Result<()> {
        let ip = identity.ip;

        // Update the main entries map
        {
            let mut entries = self.entries.write().unwrap();
            entries
                .entry(ip)
                .and_modify(|entry| entry.merge(&identity))
                .or_insert_with(|| CorrelationEntry::from_identity(&identity));
        }

        // Update hostname index
        {
            let mut hostname_index = self.hostname_index.write().unwrap();
            for hostname in &identity.hostnames {
                hostname_index.insert(hostname.to_lowercase(), ip);
            }
        }

        // Publish the updated correlation entry
        self.publish_correlation(ip).await
    }

    /// Register a device by IP and hostname only.
    ///
    /// Convenience method for bridges that don't have full identity info.
    pub async fn register_simple(
        &self,
        ip: IpAddr,
        hostname: Option<String>,
        bridge: &str,
        source_id: &str,
    ) -> Result<()> {
        let hostnames = hostname.into_iter().collect();
        self.register_device(DeviceIdentity {
            ip,
            hostnames,
            bridge: bridge.to_string(),
            source_id: source_id.to_string(),
        })
        .await
    }

    /// Look up a correlation entry by IP.
    pub fn lookup_by_ip(&self, ip: IpAddr) -> Option<CorrelationEntry> {
        let entries = self.entries.read().unwrap();
        entries.get(&ip).cloned()
    }

    /// Look up a correlation entry by hostname.
    pub fn lookup_by_hostname(&self, hostname: &str) -> Option<CorrelationEntry> {
        let hostname_lower = hostname.to_lowercase();
        let ip = {
            let hostname_index = self.hostname_index.read().unwrap();
            hostname_index.get(&hostname_lower).copied()
        };

        ip.and_then(|ip| self.lookup_by_ip(ip))
    }

    /// Get the IP address for a hostname.
    pub fn resolve_hostname(&self, hostname: &str) -> Option<IpAddr> {
        let hostname_index = self.hostname_index.read().unwrap();
        hostname_index.get(&hostname.to_lowercase()).copied()
    }

    /// Get all correlation entries.
    pub fn all_entries(&self) -> Vec<CorrelationEntry> {
        let entries = self.entries.read().unwrap();
        entries.values().cloned().collect()
    }

    /// Get the number of correlated devices.
    pub fn device_count(&self) -> usize {
        let entries = self.entries.read().unwrap();
        entries.len()
    }

    /// Publish correlation entry to Zenoh.
    async fn publish_correlation(&self, ip: IpAddr) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };

        let entry = {
            let entries = self.entries.read().unwrap();
            entries.get(&ip).cloned()
        };

        if let Some(entry) = entry {
            let key = format!("zensight/_meta/correlation/{}", ip);
            publisher.publish_json(&key, &entry).await?;
        }

        Ok(())
    }

    /// Publish all correlation entries to Zenoh.
    pub async fn publish_all(&self) -> Result<()> {
        let Some(ref publisher) = self.publisher else {
            return Ok(());
        };

        let entries: Vec<_> = {
            let entries = self.entries.read().unwrap();
            entries
                .iter()
                .map(|(ip, entry)| (*ip, entry.clone()))
                .collect()
        };

        for (ip, entry) in entries {
            let key = format!("zensight/_meta/correlation/{}", ip);
            publisher.publish_json(&key, &entry).await?;
        }

        Ok(())
    }
}

impl Default for CorrelationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Bridge discovery information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeInfo {
    /// Bridge name (e.g., "snmp", "syslog").
    pub name: String,
    /// Bridge version.
    pub version: String,
    /// Key prefix used by this bridge.
    pub key_prefix: String,
    /// Protocol handled.
    pub protocol: String,
    /// Number of devices being monitored.
    pub device_count: u64,
    /// Bridge status.
    pub status: String,
    /// Last heartbeat timestamp.
    pub last_heartbeat: i64,
}

impl BridgeInfo {
    /// Create new bridge info.
    pub fn new(name: &str, version: &str, key_prefix: &str, protocol: &str) -> Self {
        Self {
            name: name.to_string(),
            version: version.to_string(),
            key_prefix: key_prefix.to_string(),
            protocol: protocol.to_string(),
            device_count: 0,
            status: "running".to_string(),
            last_heartbeat: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Update device count.
    pub fn with_device_count(mut self, count: u64) -> Self {
        self.device_count = count;
        self
    }

    /// Publish bridge info to Zenoh.
    pub async fn publish(&self, publisher: &Publisher) -> Result<()> {
        let key = format!("zensight/_meta/bridges/{}", self.name);
        publisher.publish_json(&key, self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_registry_new() {
        let registry = CorrelationRegistry::new();
        assert_eq!(registry.device_count(), 0);
    }

    #[tokio::test]
    async fn test_register_device() {
        let registry = CorrelationRegistry::new();

        let identity = DeviceIdentity {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            hostnames: vec!["router01".to_string()],
            bridge: "snmp".to_string(),
            source_id: "router01".to_string(),
        };

        registry.register_device(identity).await.unwrap();

        assert_eq!(registry.device_count(), 1);

        let entry = registry.lookup_by_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.bridges, vec!["snmp"]);
        assert_eq!(entry.hostnames, vec!["router01"]);
    }

    #[tokio::test]
    async fn test_merge_identities() {
        let registry = CorrelationRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

        // Register from SNMP
        registry
            .register_device(DeviceIdentity {
                ip,
                hostnames: vec!["router01".to_string()],
                bridge: "snmp".to_string(),
                source_id: "router01".to_string(),
            })
            .await
            .unwrap();

        // Register from syslog with different hostname
        registry
            .register_device(DeviceIdentity {
                ip,
                hostnames: vec!["router01.local".to_string()],
                bridge: "syslog".to_string(),
                source_id: "router01.local".to_string(),
            })
            .await
            .unwrap();

        // Should still be one device
        assert_eq!(registry.device_count(), 1);

        let entry = registry.lookup_by_ip(ip).unwrap();
        assert_eq!(entry.bridges.len(), 2);
        assert!(entry.bridges.contains(&"snmp".to_string()));
        assert!(entry.bridges.contains(&"syslog".to_string()));
        assert_eq!(entry.hostnames.len(), 2);
    }

    #[tokio::test]
    async fn test_hostname_lookup() {
        let registry = CorrelationRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        registry
            .register_device(DeviceIdentity {
                ip,
                hostnames: vec!["server01".to_string(), "Server01.Local".to_string()],
                bridge: "sysinfo".to_string(),
                source_id: "server01".to_string(),
            })
            .await
            .unwrap();

        // Case-insensitive lookup
        let entry = registry.lookup_by_hostname("SERVER01");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().ip, ip);

        let entry = registry.lookup_by_hostname("server01.local");
        assert!(entry.is_some());
    }

    #[test]
    fn test_bridge_info() {
        let info = BridgeInfo::new("snmp", "0.1.0", "zensight/snmp", "snmp").with_device_count(10);

        assert_eq!(info.name, "snmp");
        assert_eq!(info.device_count, 10);
        assert_eq!(info.status, "running");
    }
}
