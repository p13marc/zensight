//! Configuration for the systemd sensor.

use serde::{Deserialize, Serialize};
use zensight_common::config::ZenohConfig;

// Re-export LoggingConfig from the framework for compatibility.
pub use zensight_sensor_core::LoggingConfig;

/// Complete sensor configuration (three-part JSON5: `zenoh` / `systemd` / `logging`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemdSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// systemd-specific settings.
    #[serde(default)]
    pub systemd: SystemdConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// On-demand debug-report (`@/report`) limits. Disabled by default.
    #[serde(default)]
    pub report: zensight_sensor_core::ReportLimits,

    /// Tier-2 directory-snapshot (`@/snapshot`) limits. Disabled by default.
    #[serde(default)]
    pub snapshot: zensight_sensor_core::SnapshotLimits,
}

/// systemd protocol configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemdConfig {
    /// Key expression prefix (default: `zensight/systemd`).
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Poll interval in seconds (default: 15).
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Source identifier override; defaults to the local hostname when empty.
    #[serde(default)]
    pub source: Option<String>,

    /// Unit-name globs to stream per-unit telemetry for (#273). Empty = none.
    /// Hundreds of units exist per host, so per-unit series are watchlist-scoped
    /// to bound key cardinality. Matched with `glob` semantics (`*`, `?`, `[…]`).
    #[serde(default)]
    pub watch_units: Vec<String>,

    /// Hard cap on how many matched units stream per-unit telemetry (#273). Excess
    /// matches are dropped (and logged — no silent truncation) and folded into the
    /// `other/*` aggregate bucket.
    #[serde(default = "default_watch_max")]
    pub watch_max: usize,

    /// Collect opt-in IP/IO accounting per watched unit (#273). Only surfaces when
    /// the unit itself enabled `IPAccounting=`/`IOAccounting=`; absent otherwise.
    #[serde(default)]
    pub ip_io_accounting: bool,

    /// Bounded capacity of the control-plane event ring (#275) served on
    /// `@/query/events`.
    #[serde(default = "default_events_capacity")]
    pub events_capacity: usize,

    /// Built-in threshold alerts (#276).
    #[serde(default)]
    pub alerts: crate::alerts::AlertsConfig,

    /// Collector toggles.
    #[serde(default)]
    pub collect: CollectConfig,
}

/// Compile `watch_units` globs, logging and skipping any invalid pattern. Shared
/// by the collector (#273) and the event stream (#275).
pub fn compile_watch(patterns: &[String]) -> Vec<glob::Pattern> {
    patterns
        .iter()
        .filter_map(|p| match glob::Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                tracing::warn!(pattern = %p, error = %e, "ignoring invalid watch_units glob");
                None
            }
        })
        .collect()
}

/// Which families the collector gathers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Enumerate units (`ListUnits`) for the `units/*` state aggregates. When off,
    /// only the cheap scalar Manager properties (`manager/*`) + boot timings are
    /// collected. Default on.
    #[serde(default = "default_true")]
    pub list_units: bool,

    /// Boot-performance timings (`boot/*`), computed once-per-tick from the Manager
    /// monotonic timestamps. Cheap; default on.
    #[serde(default = "default_true")]
    pub boot: bool,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            list_units: true,
            boot: true,
        }
    }
}

impl Default for SystemdConfig {
    fn default() -> Self {
        Self {
            key_prefix: default_key_prefix(),
            poll_interval_secs: default_poll_interval_secs(),
            source: None,
            watch_units: Vec::new(),
            watch_max: default_watch_max(),
            ip_io_accounting: false,
            events_capacity: default_events_capacity(),
            alerts: crate::alerts::AlertsConfig::default(),
            collect: CollectConfig::default(),
        }
    }
}

fn default_watch_max() -> usize {
    50
}

fn default_events_capacity() -> usize {
    256
}

fn default_key_prefix() -> String {
    "zensight/systemd".to_string()
}

fn default_poll_interval_secs() -> u64 {
    15
}

fn default_true() -> bool {
    true
}

impl SystemdSensorConfig {
    /// Resolve the telemetry `source`: the configured override, else the local
    /// hostname, else `"unknown"`.
    pub fn source(&self) -> String {
        self.systemd
            .source
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| hostname::get().ok().and_then(|h| h.into_string().ok()))
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl zensight_sensor_core::SensorConfig for SystemdSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn key_prefix(&self) -> &str {
        &self.systemd.key_prefix
    }

    fn report_limits(&self) -> zensight_sensor_core::ReportLimits {
        self.report.clone()
    }

    fn snapshot_limits(&self) -> zensight_sensor_core::SnapshotLimits {
        self.snapshot.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        if self.systemd.poll_interval_secs == 0 {
            return Err(zensight_sensor_core::SensorError::config(
                "systemd.poll_interval_secs must be greater than 0",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_sensor_core::SensorConfig;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let json = r#"{ zenoh: { mode: "peer" } }"#;
        let cfg: SystemdSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(cfg.key_prefix(), "zensight/systemd");
        assert_eq!(cfg.systemd.poll_interval_secs, 15);
        assert!(cfg.systemd.collect.list_units);
        assert!(cfg.systemd.collect.boot);
        cfg.validate().unwrap();
    }

    #[test]
    fn parses_full_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            systemd: {
                key_prefix: "zensight/systemd",
                poll_interval_secs: 30,
                source: "gw01",
                collect: { list_units: false, boot: true },
            },
        }"#;
        let cfg: SystemdSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(cfg.systemd.poll_interval_secs, 30);
        assert!(!cfg.systemd.collect.list_units);
        assert_eq!(cfg.source(), "gw01");
    }

    #[test]
    fn zero_interval_is_rejected() {
        let json = r#"{ zenoh: { mode: "peer" }, systemd: { poll_interval_secs: 0 } }"#;
        let cfg: SystemdSensorConfig = json5::from_str(json).unwrap();
        assert!(cfg.validate().is_err());
    }
}
