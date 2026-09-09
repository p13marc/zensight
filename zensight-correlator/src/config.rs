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

    /// The longest a recompute may be deferred, however busy the bus
    /// (milliseconds). `0` disables the cap and restores the pure debounce.
    ///
    /// The debounce alone is an **idle** gap, and a fleet's inbound stream —
    /// evidence refreshes, a passive-DNS observation per resolved IP from
    /// netring, every alert transition, every ack and silence, every liveliness
    /// flap — has no idle gap to find (#1106). Below the debounce the deadline
    /// slid forward on every message and `recompute` never ran, while the 60 s
    /// `reemit` republished the frozen set with a fresh `last_updated`: new
    /// hosts never appeared, retired ones never tombstoned, and the catalog
    /// said it had just recomputed.
    #[serde(default = "default_recompute_max_wait_ms")]
    pub recompute_max_wait_ms: u64,

    /// Re-publish every current entity on this cadence (seconds) — doubles as
    /// correlator liveness and seeds a late-restarted bus.
    #[serde(default = "default_reemit_secs")]
    pub reemit_secs: u64,

    /// Per-rule kill-switches.
    #[serde(default)]
    pub rules: RulesConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Allow operator identity assertions — `@catalog/@rpc/link` and `unlink`
    /// (#473, RFC 06 §5.4).
    ///
    /// **Off by default, and deliberately so.** A `link` overrides the
    /// conflicting-strong-ids guard, which is the one rule standing between the
    /// catalog and silently fusing two real machines into one host. The RFC calls
    /// the procedure "operator-invoked, gated"; this is the gate. When off, both
    /// procedures are still *served* — they reply `error/gated` rather than
    /// timing out, so an operator learns the feature exists and is switched off,
    /// instead of learning nothing.
    #[serde(default)]
    pub allow_operator_assertions: bool,

    /// Where operator decisions are kept across restarts (#1102).
    ///
    /// `link`/`unlink`, `ack` and `silence` are the catalog's **only**
    /// non-derivable state: nothing on the bus implies them and no amount of
    /// recomputation produces one. They are published as ordinary catalog
    /// state so a restart can re-seed them — but the publish had no cache
    /// behind it, the ack and silence seeds are served by *this* process (so a
    /// restart asks itself and is answered from its own empty store), and the
    /// shipped `configs/` run no router storage. An operator's `link` did not
    /// survive a restart.
    ///
    /// Empty disables persistence, which is the pre-#1102 behaviour and what
    /// every test wants.
    #[serde(default = "default_decisions_path")]
    pub operator_decisions: String,

    /// Incident evaluation (#900): group firing alerts by entity, attribute
    /// them over the relationship graph, and publish
    /// `@catalog/state/incident/*`.
    ///
    /// **On by default**, unlike `allow_operator_assertions`, and the
    /// difference is the point: assertions let an operator override the one
    /// guard standing between the catalog and fusing two real machines, so
    /// they are off until someone chooses them. Incidents only *read* — they
    /// group alerts that already exist and publish a conclusion, exactly as
    /// entity and edge documents do, and turning them off makes the fleet's
    /// triage surface silently absent rather than safe.
    ///
    /// The switch exists because the mechanism that could misbehave must be
    /// disarmable from outside itself: an incident pass that churns or leaks
    /// should be stoppable without stopping the catalog.
    #[serde(default = "default_true")]
    pub incidents_enabled: bool,
}

fn default_evidence_ttl() -> u64 {
    900
}

fn default_recompute_debounce_ms() -> u64 {
    500
}

fn default_decisions_path() -> String {
    // Beside the daemon's other state; the systemd unit's `StateDirectory=`
    // puts this under /var/lib/zensight-correlator.
    "/var/lib/zensight-correlator/operator-decisions.json5".to_string()
}

fn default_recompute_max_wait_ms() -> u64 {
    2_000
}

fn default_reemit_secs() -> u64 {
    60
}

impl Default for CorrelatorConfig {
    fn default() -> Self {
        Self {
            zenoh: ZenohConfig::default(),
            serialization: Format::default(),
            evidence_ttl_secs: default_evidence_ttl(),
            recompute_debounce_ms: default_recompute_debounce_ms(),
            recompute_max_wait_ms: default_recompute_max_wait_ms(),
            reemit_secs: default_reemit_secs(),
            rules: RulesConfig::default(),
            logging: LoggingConfig::default(),
            allow_operator_assertions: false,
            operator_decisions: default_decisions_path(),
            incidents_enabled: true,
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
    /// Merge on equal cloud `(provider, instance_id)` (authoritative per
    /// provider, #311). Default: true.
    #[serde(default = "default_true")]
    pub cloud_instance: bool,
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
            cloud_instance: true,
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
        assert_eq!(config.recompute_max_wait_ms, 2_000);
        assert_eq!(config.reemit_secs, 60);
        assert!(config.rules.host_id);
        assert!(config.rules.cloud_instance);
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
            rules: { hostname_enabled: false },
        }"#;
        let config = CorrelatorConfig::parse(json).unwrap();
        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.serialization, Format::Cbor);
        assert_eq!(config.evidence_ttl_secs, 600);
        assert_eq!(config.recompute_debounce_ms, 250);
        assert_eq!(config.reemit_secs, 30);
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
        assert_eq!(d.recompute_max_wait_ms, 2_000);
        assert_eq!(d.reemit_secs, 60);
        assert!(d.rules.hostname_enabled);
    }
}
