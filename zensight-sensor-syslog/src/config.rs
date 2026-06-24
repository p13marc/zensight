//! Syslog sensor configuration.

use crate::filter::SyslogFilterConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;
use zensight_common::config::ZenohConfig;

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_sensor_core::LoggingConfig;

/// Complete syslog sensor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Syslog-specific settings.
    pub syslog: SyslogConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Syslog receiver configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyslogConfig {
    /// Key expression prefix for publishing.
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Listener configurations.
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,

    /// Hostname overrides for source identification.
    #[serde(default)]
    pub hostname_aliases: std::collections::HashMap<String, String>,

    /// Whether to include raw message in labels.
    #[serde(default)]
    pub include_raw_message: bool,

    /// Message filtering configuration.
    #[serde(default)]
    pub filter: SyslogFilterConfig,

    /// Enable dynamic filter commands via Zenoh.
    #[serde(default)]
    pub enable_dynamic_filters: bool,

    /// systemd-journald ingestion (#57). Reads the local journal directly via
    /// libsystemd (no `journalctl` subprocess) and feeds the same pipeline as
    /// the network listeners. `None` (the default) leaves journald disabled.
    #[serde(default)]
    pub journald: Option<JournaldConfig>,
}

/// systemd-journald source configuration.
///
/// Minimal by design: `{ "enabled": true }` tails the local system journal with
/// sane defaults. Cursor resume (#58) and server-side matching (#59) extend this.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
// Fields beyond `enabled` are read only by the (feature-gated) journald reader.
#[cfg_attr(not(feature = "journald"), allow(dead_code))]
pub struct JournaldConfig {
    /// Master switch. When false the reader is not started.
    #[serde(default)]
    pub enabled: bool,

    /// Which journal to open.
    #[serde(default)]
    pub scope: JournaldScope,

    /// Open a specific journald log namespace instead of the default journal.
    #[serde(default)]
    pub namespace: Option<String>,

    /// Where to begin reading on startup (#58). Defaults to resuming from the
    /// persisted cursor (first run behaves like `tail`).
    #[serde(default)]
    pub start_from: StartFrom,

    /// Lookback window for `start_from: "since"`, e.g. `"15m"`, `"1h"`, `"2d"`.
    #[serde(default)]
    pub since: Option<String>,

    /// Path of the cursor state file. `None` picks a sensible default
    /// (`$STATE_DIRECTORY/journald.cursor` under systemd, else an XDG state dir).
    #[serde(default)]
    pub cursor_file: Option<std::path::PathBuf>,

    /// What to do when `start_from: "cursor"` but the saved cursor is gone
    /// (rotated out): start from the tail, or from `since`.
    #[serde(default)]
    pub on_missing_cursor: MissingCursor,

    /// Server-side filter: only these systemd units (`_SYSTEMD_UNIT`), OR'd.
    /// Empty = all units. Applied in the journal itself (#59), so filtered
    /// entries are never decoded or transported.
    #[serde(default)]
    pub units: Vec<String>,

    /// Server-side filter: minimum priority 0..=7 (3 = err). Expands to a
    /// `PRIORITY=0..min` OR-group (libsystemd has no `<=` match). `None` = all.
    #[serde(default)]
    pub min_priority: Option<u8>,

    /// Server-side filter: only these transports (`_TRANSPORT`, e.g. `kernel`,
    /// `journal`, `stdout`, `syslog`), OR'd. Empty = all.
    #[serde(default)]
    pub transports: Vec<String>,

    /// Server-side filter: raw `FIELD=value` matches, AND'd with the above
    /// (same-field entries OR per libsystemd semantics). Escape hatch for
    /// arbitrary journald fields.
    #[serde(default, rename = "match")]
    pub match_fields: std::collections::HashMap<String, String>,

    /// Extra raw journald field names (e.g. `_SELINUX_CONTEXT`) to copy verbatim
    /// into labels, on top of the standard set (unit, pid, comm, boot_id, …).
    #[serde(default)]
    pub extra_fields: Vec<String>,

    /// Include developer/code-location fields (CODE_FILE/CODE_LINE/CODE_FUNC,
    /// ERRNO). Off by default to keep label cardinality bounded.
    #[serde(default)]
    pub include_dev_fields: bool,
}

/// Where the journald reader begins on startup (#58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartFrom {
    /// Resume from the persisted cursor; first run behaves like `tail`.
    #[default]
    Cursor,
    /// Only entries newer than startup.
    Tail,
    /// Replay the entire journal from the beginning (can be large).
    Head,
    /// Only entries from the current boot.
    Boot,
    /// Entries within the `since` lookback window.
    Since,
}

/// Fallback when a saved cursor can no longer be resolved (#58).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingCursor {
    /// Start from the tail (only new entries).
    #[default]
    Tail,
    /// Start from the `since` lookback window.
    Since,
}

/// Which systemd journal to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournaldScope {
    /// System services and the kernel (default; needs journal-read access).
    #[default]
    System,
    /// The invoking user's journal (always readable unprivileged).
    User,
    /// Only local journal files (exclude remote/uploaded journals).
    LocalOnly,
    /// Only volatile runtime journals (`/run`), not persisted ones.
    RuntimeOnly,
}

fn default_key_prefix() -> String {
    "zensight/syslog".to_string()
}

/// Individual listener configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
    /// Protocol: "udp", "tcp", or "unix".
    pub protocol: ListenerProtocol,

    /// Bind address.
    /// - For UDP/TCP: "0.0.0.0:514"
    /// - For Unix: "/var/run/syslog.sock"
    pub bind: String,

    /// Maximum message size in bytes (UDP only).
    #[serde(default = "default_max_message_size")]
    pub max_message_size: usize,

    /// TCP/Unix: maximum concurrent connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// TCP/Unix: connection timeout in seconds.
    #[serde(default = "default_connection_timeout_secs")]
    pub connection_timeout_secs: u64,

    /// Unix socket: file permissions (octal, e.g., 0o666 = 438).
    #[serde(default = "default_socket_mode")]
    pub socket_mode: u32,

    /// Unix socket: remove existing socket file before binding.
    #[serde(default = "default_true")]
    pub remove_existing_socket: bool,
}

fn default_max_message_size() -> usize {
    65535
}

fn default_max_connections() -> usize {
    1000
}

fn default_connection_timeout_secs() -> u64 {
    300
}

fn default_socket_mode() -> u32 {
    0o666
}

fn default_true() -> bool {
    true
}

/// Listener protocol type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListenerProtocol {
    Udp,
    Tcp,
    Unix,
}

impl std::fmt::Display for ListenerProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Udp => write!(f, "udp"),
            Self::Tcp => write!(f, "tcp"),
            Self::Unix => write!(f, "unix"),
        }
    }
}

impl SyslogSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let config: Self = json5::from_str(&content)?;
        config.validate_config()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate_config(&self) -> anyhow::Result<()> {
        // A source is required: at least one network listener OR journald.
        let journald_enabled = self.syslog.journald.as_ref().is_some_and(|j| j.enabled);
        if self.syslog.listeners.is_empty() && !journald_enabled {
            anyhow::bail!("No source configured: add at least one listener or enable journald");
        }

        for (i, listener) in self.syslog.listeners.iter().enumerate() {
            if listener.bind.is_empty() {
                anyhow::bail!("Listener {} has empty bind address", i);
            }

            match listener.protocol {
                ListenerProtocol::Udp | ListenerProtocol::Tcp => {
                    // Validate bind address format for network protocols
                    if !listener.bind.contains(':') {
                        anyhow::bail!(
                            "Listener {} bind address must include port (e.g., '0.0.0.0:514')",
                            i
                        );
                    }
                }
                ListenerProtocol::Unix => {
                    // Unix socket path should be absolute or relative path
                    // Just check it's not empty (already done above)
                }
            }
        }

        Ok(())
    }
}

impl zensight_sensor_core::SensorConfig for SyslogSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn key_prefix(&self) -> &str {
        &self.syslog.key_prefix
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        self.validate_config()
            .map_err(|e| zensight_sensor_core::SensorError::config(e.to_string()))
    }
}

impl Default for SyslogConfig {
    fn default() -> Self {
        Self {
            key_prefix: default_key_prefix(),
            listeners: vec![ListenerConfig {
                protocol: ListenerProtocol::Udp,
                bind: "0.0.0.0:514".to_string(),
                max_message_size: default_max_message_size(),
                max_connections: default_max_connections(),
                connection_timeout_secs: default_connection_timeout_secs(),
                socket_mode: default_socket_mode(),
                remove_existing_socket: default_true(),
            }],
            hostname_aliases: std::collections::HashMap::new(),
            include_raw_message: false,
            filter: SyslogFilterConfig::default(),
            enable_dynamic_filters: false,
            journald: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514" }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.key_prefix, "zensight/syslog");
        assert_eq!(config.syslog.listeners.len(), 1);
        assert_eq!(config.syslog.listeners[0].protocol, ListenerProtocol::Udp);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            syslog: {
                key_prefix: "custom/syslog",
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514", max_message_size: 8192 },
                    { protocol: "tcp", bind: "0.0.0.0:514", max_connections: 500 }
                ],
                hostname_aliases: {
                    "192.168.1.1": "router01"
                },
                include_raw_message: true
            },
            logging: {
                level: "debug"
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.key_prefix, "custom/syslog");
        assert_eq!(config.syslog.listeners.len(), 2);
        assert_eq!(config.syslog.listeners[0].max_message_size, 8192);
        assert_eq!(config.syslog.listeners[1].max_connections, 500);
        assert_eq!(
            config.syslog.hostname_aliases.get("192.168.1.1"),
            Some(&"router01".to_string())
        );
        assert!(config.syslog.include_raw_message);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_parse_unix_socket_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    {
                        protocol: "unix",
                        bind: "/var/run/syslog.sock",
                        socket_mode: 438,
                        remove_existing_socket: true
                    }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.listeners.len(), 1);
        assert_eq!(config.syslog.listeners[0].protocol, ListenerProtocol::Unix);
        assert_eq!(config.syslog.listeners[0].bind, "/var/run/syslog.sock");
        assert_eq!(config.syslog.listeners[0].socket_mode, 438); // 0o666
        assert!(config.syslog.listeners[0].remove_existing_socket);
        assert!(config.validate_config().is_ok());
    }

    #[test]
    fn test_parse_filter_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0:514" }
                ],
                filter: {
                    min_severity: 4,
                    exclude_facilities: ["local7"],
                    exclude_app_patterns: [
                        { pattern: "systemd-*", pattern_type: "glob" }
                    ]
                },
                enable_dynamic_filters: true
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.syslog.filter.min_severity, Some(4));
        assert_eq!(config.syslog.filter.exclude_facilities, vec!["local7"]);
        assert_eq!(config.syslog.filter.exclude_app_patterns.len(), 1);
        assert!(config.syslog.enable_dynamic_filters);
    }

    #[test]
    fn test_validate_empty_listeners() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: []
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_validate_missing_port() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "udp", bind: "0.0.0.0" }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_validate_unix_no_port_required() {
        let json = r#"{
            zenoh: { mode: "peer" },
            syslog: {
                listeners: [
                    { protocol: "unix", bind: "/tmp/syslog.sock" }
                ]
            }
        }"#;

        let config: SyslogSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_ok());
    }
}
