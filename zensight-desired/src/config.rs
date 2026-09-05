//! The daemon's own file config (#938).
//!
//! Small on purpose. Everything that decides *what* the fleet gets lives in
//! the policy file, which is reviewed and versioned; this decides only how to
//! reach the bus and how often to look.

use serde::{Deserialize, Serialize};
use zensight_common::config::{LoggingConfig, ZenohConfig};
use zensight_common::serialization::Format;

/// `configs/desired.json5`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredDaemonConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub serialization: Format,
    #[serde(default)]
    pub desired: ControllerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerConfig {
    /// Path to `fleet-policy.json5`.
    #[serde(default = "default_policy_path")]
    pub policy: String,
    /// How often `run` re-evaluates. The catalog subscription is the
    /// accelerator; this is the level-triggered floor that survives a missed
    /// sample, exactly as the sensor-side reconciler's re-GET does.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
    /// How many refresh periods a document must be unproduced before it is
    /// deleted.
    ///
    /// Never zero. A document is deleted only when the policy *stops yielding
    /// it* for a host the catalog still shows — never because the catalog
    /// stopped showing the host. Without the grace, one slow catalog pass
    /// during a restart would delete the fleet's configuration, and every
    /// sensor would revert to its file baseline at once.
    #[serde(default = "default_delete_grace")]
    pub delete_grace_periods: u32,
    /// Where `override/set` records what a GUI adopted (#939).
    ///
    /// A separate file from the policy, and the separation is the design: the
    /// policy is hand-written and commented, and a serde round trip would
    /// strip every comment and normalise the class order — which IS the
    /// overlay order. A machine may only write a file whose whole content it
    /// owns.
    #[serde(default = "default_overrides_path")]
    pub overrides: String,
    /// Whether `override/set` is answered or refused.
    ///
    /// Off by default. An override changes what a host is told to do on an
    /// operator's say-so, and is **durable** — it outlives the session that
    /// made it — so it is the same class of thing as the catalog's
    /// `link`/`unlink`, and gets the same treatment: still served when off,
    /// replying `error/gated`, so an operator learns the feature exists and is
    /// switched off rather than learning nothing from a timeout.
    #[serde(default)]
    pub allow_overrides: bool,
    /// Refuse to publish, whatever the policy says. The disarm switch lives
    /// here rather than in the policy for the same reason the sensor's does:
    /// the mechanism that could misbehave must be disarmable from outside
    /// itself.
    #[serde(default)]
    pub dry_run: bool,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        ControllerConfig {
            policy: default_policy_path(),
            overrides: default_overrides_path(),
            allow_overrides: false,
            refresh_secs: default_refresh_secs(),
            delete_grace_periods: default_delete_grace(),
            dry_run: false,
        }
    }
}

fn default_policy_path() -> String {
    "/etc/zensight/fleet-policy.json5".to_string()
}
fn default_overrides_path() -> String {
    "/etc/zensight/fleet-policy.overrides.json5".to_string()
}
fn default_refresh_secs() -> u64 {
    300
}
fn default_delete_grace() -> u32 {
    2
}

impl DesiredDaemonConfig {
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let cfg: Self = json5::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        if self.desired.refresh_secs == 0 {
            return Err("desired.refresh_secs must be > 0 — a zero period is a spin".into());
        }
        if self.desired.delete_grace_periods == 0 {
            return Err(
                "desired.delete_grace_periods must be > 0: with no grace, one slow catalog \
                 pass deletes the fleet's configuration and every sensor reverts to its file \
                 baseline at once"
                    .into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_grace_is_refused_with_the_reason() {
        let cfg = DesiredDaemonConfig {
            desired: ControllerConfig {
                delete_grace_periods: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let err = cfg.validate().expect_err("zero grace");
        assert!(err.contains("reverts to its file baseline"), "{err}");
    }

    #[test]
    fn the_shipped_config_loads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/desired.json5");
        DesiredDaemonConfig::load(path).expect("configs/desired.json5 must load");
    }
}
