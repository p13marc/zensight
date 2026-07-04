//! Configuration for the correlator.

use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zensight_common::config::{LoggingConfig, ZenohConfig};
use zensight_common::serialization::Format;

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

/// Complete correlator configuration.
///
/// `Default` is implemented by hand (not derived) so it matches the serde
/// per-field defaults — a derived `Default` would give `evidence_ttl_secs = 0`
/// etc., which would make a config-less run (or any `CorrelatorConfig::default()`)
/// silently age out all evidence immediately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelatorConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// Serialization format for published entity docs.
    #[serde(default)]
    pub serialization: Format,

    /// Evidence older than this (seconds) is ignored by merge rules and swept
    /// from the store. Publishers refresh live claims at ≤ TTL/2.
    #[serde(default = "default_evidence_ttl")]
    pub evidence_ttl_secs: u64,

    /// Coalesce a burst of incoming evidence into one recompute after this idle
    /// gap (milliseconds).
    #[serde(default = "default_recompute_debounce_ms")]
    pub recompute_debounce_ms: u64,

    /// Re-publish every current entity on this cadence (seconds) — doubles as
    /// correlator liveness and seeds a late-restarted bus.
    #[serde(default = "default_reemit_secs")]
    pub reemit_secs: u64,

    /// Roll device-liveness status onto entities (worst-of-members). Disable if
    /// the extra liveness subscription is unwanted.
    #[serde(default = "default_status_from_liveness")]
    pub status_from_liveness: bool,

    /// Per-rule kill-switches.
    #[serde(default)]
    pub rules: RulesConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

fn default_evidence_ttl() -> u64 {
    900
}

fn default_recompute_debounce_ms() -> u64 {
    500
}

fn default_reemit_secs() -> u64 {
    60
}

fn default_status_from_liveness() -> bool {
    true
}

impl Default for CorrelatorConfig {
    fn default() -> Self {
        Self {
            zenoh: ZenohConfig::default(),
            serialization: Format::default(),
            evidence_ttl_secs: default_evidence_ttl(),
            recompute_debounce_ms: default_recompute_debounce_ms(),
            reemit_secs: default_reemit_secs(),
            status_from_liveness: default_status_from_liveness(),
            rules: RulesConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

/// Merge-rule kill-switches. Weaker rules are more false-merge-prone; the
/// hostname rule (weakest, twenty `MacBook-Pro.local`s) can be turned off
/// independently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesConfig {
    /// Merge on equal hashed machine-id (certain). Default: true.
    #[serde(default = "default_true")]
    pub host_id: bool,
    /// Merge on a shared MAC *and* a shared IP (strong). Default: true.
    #[serde(default = "default_true")]
    pub mac_ip: bool,
    /// Merge on equal FQDN (medium). Default: true.
    #[serde(default = "default_true")]
    pub fqdn: bool,
    /// Merge on equal bare hostname (weak, false-prone). Default: true.
    #[serde(default = "default_true")]
    pub hostname_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            host_id: true,
            mac_ip: true,
            fqdn: true,
            hostname_enabled: true,
        }
    }
}

impl CorrelatorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: CorrelatorConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse configuration from a JSON5 string.
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: CorrelatorConfig = json5::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.evidence_ttl_secs == 0 {
            return Err(ConfigError::Validation(
                "evidence_ttl_secs must be > 0".to_string(),
            ));
        }
        if self.reemit_secs == 0 {
            return Err(ConfigError::Validation(
                "reemit_secs must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_defaults() {
        let config = CorrelatorConfig::parse("{}").unwrap();
        assert_eq!(config.zenoh.mode, "peer");
        assert_eq!(config.evidence_ttl_secs, 900);
        assert_eq!(config.recompute_debounce_ms, 500);
        assert_eq!(config.reemit_secs, 60);
        assert!(config.status_from_liveness);
        assert!(config.rules.host_id);
        assert!(config.rules.mac_ip);
        assert!(config.rules.fqdn);
        assert!(config.rules.hostname_enabled);
    }

    #[test]
    fn full_config_parses() {
        let json = r#"{
            zenoh: { mode: "client", connect: ["tcp/localhost:7447"] },
            serialization: "cbor",
            evidence_ttl_secs: 600,
            recompute_debounce_ms: 250,
            reemit_secs: 30,
            status_from_liveness: false,
            rules: { hostname_enabled: false },
        }"#;
        let config = CorrelatorConfig::parse(json).unwrap();
        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.serialization, Format::Cbor);
        assert_eq!(config.evidence_ttl_secs, 600);
        assert_eq!(config.recompute_debounce_ms, 250);
        assert_eq!(config.reemit_secs, 30);
        assert!(!config.status_from_liveness);
        // Unspecified rule fields keep their default (true); the named one flips.
        assert!(config.rules.host_id);
        assert!(!config.rules.hostname_enabled);
    }

    #[test]
    fn zero_ttl_rejected() {
        assert!(CorrelatorConfig::parse(r#"{ evidence_ttl_secs: 0 }"#).is_err());
    }

    #[test]
    fn default_matches_serde_defaults() {
        // A hand-written Default must agree with parsing "{}" — a derived Default
        // would give evidence_ttl_secs = 0 and break config-less runs.
        let d = CorrelatorConfig::default();
        assert_eq!(d.evidence_ttl_secs, 900);
        assert_eq!(d.recompute_debounce_ms, 500);
        assert_eq!(d.reemit_secs, 60);
        assert!(d.status_from_liveness);
        assert!(d.rules.hostname_enabled);
    }
}
