//! Dynamic filter command protocol.
//!
//! Enables runtime filter updates via Zenoh pub/sub.

use crate::filter::{DynamicFilterInfo, FilterStatsSnapshot, SyslogFilterConfig};
use serde::{Deserialize, Serialize};

/// Key expression for filter commands.
/// The `@` in the path indicates an administrative/control channel.
pub fn command_key(prefix: &str) -> String {
    format!("{}/@/commands/filter", prefix)
}

/// Key expression for filter status queries.
pub fn status_key(prefix: &str) -> String {
    format!("{}/@/status/filter", prefix)
}

/// Filter command sent from frontend to bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilterCommand {
    /// Add or update a dynamic filter.
    AddFilter {
        /// Filter ID (auto-generated if not provided).
        #[serde(default)]
        id: Option<String>,
        /// Filter configuration.
        filter: SyslogFilterConfig,
    },
    /// Remove a dynamic filter by ID.
    RemoveFilter {
        /// Filter ID to remove.
        id: String,
    },
    /// Clear all dynamic filters.
    ClearFilters,
    /// Request current filter status.
    GetStatus,
}

/// Filter status response from bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterStatus {
    /// Base filter from configuration.
    pub base_filter: SyslogFilterConfig,
    /// Currently active dynamic filters.
    pub dynamic_filters: Vec<DynamicFilterInfo>,
    /// Filter statistics.
    pub stats: FilterStatsSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_key() {
        assert_eq!(
            command_key("zensight/syslog"),
            "zensight/syslog/@/commands/filter"
        );
    }

    #[test]
    fn test_status_key() {
        assert_eq!(
            status_key("zensight/syslog"),
            "zensight/syslog/@/status/filter"
        );
    }

    #[test]
    fn test_serialize_add_filter() {
        let cmd = FilterCommand::AddFilter {
            id: Some("filter1".to_string()),
            filter: SyslogFilterConfig {
                min_severity: Some(4),
                ..Default::default()
            },
        };

        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("add_filter"));
        assert!(json.contains("filter1"));
        assert!(json.contains("min_severity"));
    }

    #[test]
    fn test_serialize_remove_filter() {
        let cmd = FilterCommand::RemoveFilter {
            id: "filter1".to_string(),
        };

        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("remove_filter"));
        assert!(json.contains("filter1"));
    }

    #[test]
    fn test_deserialize_commands() {
        let json = r#"{"type": "clear_filters"}"#;
        let cmd: FilterCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, FilterCommand::ClearFilters));

        let json = r#"{"type": "get_status"}"#;
        let cmd: FilterCommand = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, FilterCommand::GetStatus));
    }
}
