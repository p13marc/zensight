//! Configuration for the sysinfo bridge.

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use zensight_common::config::ZenohConfig;

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    Parse(#[from] json5::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

/// Complete bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysinfoBridgeConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Sysinfo collection settings.
    pub sysinfo: SysinfoConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// System information collection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysinfoConfig {
    /// Key expression prefix (default: "zensight/sysinfo").
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Hostname to use in key expressions.
    /// Use "auto" to detect automatically (default).
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Poll interval in seconds (default: 5).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Which metrics to collect.
    #[serde(default)]
    pub collect: CollectConfig,

    /// Network interface filters.
    #[serde(default)]
    pub network: NetworkConfig,

    /// Disk mount filters.
    #[serde(default)]
    pub disk: DiskConfig,
}

fn default_key_prefix() -> String {
    "zensight/sysinfo".to_string()
}

fn default_hostname() -> String {
    "auto".to_string()
}

fn default_poll_interval() -> u64 {
    5
}

/// Configuration for which metrics to collect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Collect CPU metrics (usage per core, frequency).
    #[serde(default = "default_true")]
    pub cpu: bool,

    /// Collect memory metrics (used, available, swap).
    #[serde(default = "default_true")]
    pub memory: bool,

    /// Collect disk metrics (usage per mount point).
    #[serde(default = "default_true")]
    pub disk: bool,

    /// Collect network metrics (bytes/packets in/out per interface).
    #[serde(default = "default_true")]
    pub network: bool,

    /// Collect system info (uptime, load averages).
    #[serde(default = "default_true")]
    pub system: bool,

    /// Collect process metrics (top N by CPU/memory).
    /// Can be resource-intensive on systems with many processes.
    #[serde(default)]
    pub processes: bool,

    /// Number of top processes to report (default: 10).
    #[serde(default = "default_top_processes")]
    pub top_processes: usize,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            cpu: true,
            memory: true,
            disk: true,
            network: true,
            system: true,
            processes: false,
            top_processes: 10,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_top_processes() -> usize {
    10
}

/// Network interface filtering configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Only include these interfaces (empty = include all).
    #[serde(default)]
    pub include: Vec<String>,

    /// Exclude these interfaces.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Exclude loopback interfaces (default: true).
    #[serde(default = "default_true")]
    pub exclude_loopback: bool,

    /// Exclude virtual interfaces (docker, veth, etc.) (default: false).
    #[serde(default)]
    pub exclude_virtual: bool,
}

/// Disk mount filtering configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskConfig {
    /// Only include these mount points (empty = include all).
    #[serde(default)]
    pub include: Vec<String>,

    /// Exclude these mount points.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Exclude pseudo filesystems (tmpfs, devtmpfs, etc.) (default: true).
    #[serde(default = "default_true")]
    pub exclude_pseudo: bool,
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error".
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

impl SysinfoBridgeConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: SysinfoBridgeConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sysinfo.poll_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "poll_interval_secs must be > 0".to_string(),
            ));
        }

        // At least one metric type should be enabled
        let collect = &self.sysinfo.collect;
        if !collect.cpu
            && !collect.memory
            && !collect.disk
            && !collect.network
            && !collect.system
            && !collect.processes
        {
            return Err(ConfigError::Validation(
                "At least one metric type must be enabled".to_string(),
            ));
        }

        Ok(())
    }

    /// Get the hostname to use, resolving "auto" if needed.
    pub fn get_hostname(&self) -> String {
        if self.sysinfo.hostname == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.sysinfo.hostname.clone()
        }
    }
}

impl NetworkConfig {
    /// Check if an interface should be included.
    pub fn should_include(&self, name: &str) -> bool {
        // Check explicit include list
        if !self.include.is_empty() && !self.include.iter().any(|i| i == name) {
            return false;
        }

        // Check exclude list
        if self.exclude.iter().any(|e| e == name) {
            return false;
        }

        // Check loopback
        if self.exclude_loopback && name == "lo" {
            return false;
        }

        // Check virtual interfaces
        if self.exclude_virtual {
            let virtual_prefixes = ["docker", "veth", "br-", "virbr", "vnet"];
            if virtual_prefixes.iter().any(|p| name.starts_with(p)) {
                return false;
            }
        }

        true
    }
}

impl DiskConfig {
    /// Check if a mount point should be included.
    pub fn should_include(&self, mount_point: &str, fs_type: &str) -> bool {
        // Check explicit include list
        if !self.include.is_empty() && !self.include.iter().any(|i| i == mount_point) {
            return false;
        }

        // Check exclude list
        if self.exclude.iter().any(|e| e == mount_point) {
            return false;
        }

        // Check pseudo filesystems
        if self.exclude_pseudo {
            let pseudo_types = [
                "tmpfs",
                "devtmpfs",
                "devfs",
                "sysfs",
                "proc",
                "cgroup",
                "cgroup2",
                "securityfs",
                "debugfs",
                "configfs",
                "fusectl",
                "hugetlbfs",
                "mqueue",
                "pstore",
                "binfmt_misc",
                "autofs",
                "overlay",
                "squashfs",
            ];
            if pseudo_types.iter().any(|t| *t == fs_type) {
                return false;
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {}
        }"#;

        let config: SysinfoBridgeConfig = json5::from_str(json).unwrap();
        assert_eq!(config.sysinfo.key_prefix, "zensight/sysinfo");
        assert_eq!(config.sysinfo.hostname, "auto");
        assert_eq!(config.sysinfo.poll_interval_secs, 5);
        assert!(config.sysinfo.collect.cpu);
        assert!(config.sysinfo.collect.memory);
        assert!(!config.sysinfo.collect.processes);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {
                key_prefix: "metrics/host",
                hostname: "server01",
                poll_interval_secs: 10,
                collect: {
                    cpu: true,
                    memory: true,
                    disk: true,
                    network: true,
                    system: true,
                    processes: true,
                    top_processes: 5
                },
                network: {
                    exclude: ["docker0"],
                    exclude_loopback: true,
                    exclude_virtual: true
                },
                disk: {
                    exclude: ["/boot"],
                    exclude_pseudo: true
                }
            }
        }"#;

        let config: SysinfoBridgeConfig = json5::from_str(json).unwrap();
        config.validate().unwrap();

        assert_eq!(config.sysinfo.hostname, "server01");
        assert_eq!(config.sysinfo.poll_interval_secs, 10);
        assert!(config.sysinfo.collect.processes);
        assert_eq!(config.sysinfo.collect.top_processes, 5);
        assert!(config.sysinfo.network.exclude_virtual);
    }

    #[test]
    fn test_validate_zero_interval() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: { poll_interval_secs: 0 }
        }"#;

        let config: SysinfoBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_no_metrics() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {
                collect: {
                    cpu: false,
                    memory: false,
                    disk: false,
                    network: false,
                    system: false,
                    processes: false
                }
            }
        }"#;

        let config: SysinfoBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_network_filter() {
        let config = NetworkConfig {
            include: vec![],
            exclude: vec!["docker0".to_string()],
            exclude_loopback: true,
            exclude_virtual: true,
        };

        assert!(config.should_include("eth0"));
        assert!(!config.should_include("lo"));
        assert!(!config.should_include("docker0"));
        assert!(!config.should_include("veth123"));
        assert!(!config.should_include("br-abc"));
    }

    #[test]
    fn test_disk_filter() {
        let config = DiskConfig {
            include: vec![],
            exclude: vec!["/boot".to_string()],
            exclude_pseudo: true,
        };

        assert!(config.should_include("/", "ext4"));
        assert!(config.should_include("/home", "ext4"));
        assert!(!config.should_include("/boot", "ext4"));
        assert!(!config.should_include("/run", "tmpfs"));
        assert!(!config.should_include("/sys", "sysfs"));
    }
}
