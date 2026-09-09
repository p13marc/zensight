//! Configuration traits and utilities.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Result, SensorError};
use crate::{LoggingConfig, ZenohConfig};
use zensight_common::ArtifactLimits;

/// The declared resource envelope (#811/#1091).
///
/// One shape for every producer. Before #1091 this struct existed twice — as
/// `ResourcesConfig` in the netring sensor and `ResourceConfig` (singular) in
/// the historian, at two different nesting depths — and the other fourteen
/// sensors had no field at all, so `docs/ops/SIZING.md`'s instruction to "set
/// all three" named a key that nine of eleven shipped units could not accept.
///
/// Nothing here sets `deny_unknown_fields`, which is why the omission was
/// silent: a `resources` block in a config the binary did not know about
/// parsed clean and was discarded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcesConfig {
    /// Declared RSS budget, in **MiB**. Absent means *undeclared* — no
    /// `sensor-budget` alert and no shed ladder — never "unlimited and fine".
    ///
    /// The key is `budget_rss_mb`. `budget_bytes` is the internal spelling
    /// (this module's accessor and the health doc's `self_stats` field) and is
    /// not settable; the two were confused in the docs until #1091.
    #[serde(default)]
    pub budget_rss_mb: Option<u64>,
}

impl ResourcesConfig {
    /// The budget in bytes, or `None` when undeclared. The MiB -> bytes
    /// conversion for the whole tree lives here; it was duplicated in two
    /// crates before #1091.
    pub fn budget_bytes(&self) -> Option<u64> {
        self.budget_rss_mb.map(|mb| mb.saturating_mul(1024 * 1024))
    }
}

/// The undeclared envelope, so `SensorConfig::resources` can hand out a
/// reference without every implementor owning a field.
const NO_RESOURCES: &ResourcesConfig = &ResourcesConfig {
    budget_rss_mb: None,
};

/// Trait for sensor configuration types.
///
/// Implement this trait for your sensor's configuration struct to get
/// automatic loading, validation, and access to common config fields.
///
/// # Example
///
/// ```ignore
/// use serde::Deserialize;
/// use zensight_sensor_core::{SensorConfig, ZenohConfig, LoggingConfig};
///
/// #[derive(Debug, Deserialize)]
/// pub struct MySensorConfig {
///     pub zenoh: ZenohConfig,
///     pub logging: LoggingConfig,
///     pub my_protocol: MyProtocolConfig,
/// }
///
/// impl SensorConfig for MySensorConfig {
///     fn zenoh(&self) -> &ZenohConfig {
///         &self.zenoh
///     }
///
///     fn logging(&self) -> &LoggingConfig {
///         &self.logging
///     }
///
///     fn producer(&self) -> &str {
///         "my_protocol"
///     }
///
///     fn validate(&self) -> Result<()> {
///         if self.my_protocol.devices.is_empty() {
///             return Err(SensorError::validation("At least one device required"));
///         }
///         Ok(())
///     }
/// }
/// ```
pub trait SensorConfig: Sized + DeserializeOwned {
    /// Get the Zenoh configuration.
    fn zenoh(&self) -> &ZenohConfig;

    /// Get the logging configuration.
    fn logging(&self) -> &LoggingConfig;

    /// The producer name ("netlink", "logs", …) — the registry chunk this
    /// sensor publishes under. A constant per crate; the legacy config
    /// `key_prefix` is retired (#465).
    fn producer(&self) -> &str;

    /// Unified artifact-channel limits (`artifacts.{report,snapshot}`). Every kind
    /// is disabled by default; a sensor opts in by overriding this to return its
    /// configured [`ArtifactLimits`] and calling `with_artifacts` in `main`.
    fn artifact_limits(&self) -> ArtifactLimits {
        ArtifactLimits::default()
    }

    /// Identity-envelope options (`identity.*`, #311). The default keeps the
    /// cloud-metadata (IMDS) probe **off** — it makes network requests, so a
    /// sensor opts in by carrying an [`IdentityConfig`] in its config and
    /// overriding this to return it. Container-id detection needs no knob (it
    /// is a local file read, on by default with `with_identity`).
    fn identity_config(&self) -> zensight_common::IdentityConfig {
        zensight_common::IdentityConfig::default()
    }

    /// The declared resource envelope (`resources.*`, #811/#1091). Every
    /// producer carries one; the default is the undeclared envelope, for a
    /// config type that predates the block.
    fn resources(&self) -> &ResourcesConfig {
        NO_RESOURCES
    }

    /// Declared memory budget in bytes (#811) — carried into the health
    /// doc's `self_stats.budget_bytes` and graded by the runner's
    /// `sensor-budget` rule at 80 %. **Declared, not enforced** (#812 is the
    /// enforcement ladder). Absent reads as *undeclared*, never as
    /// unlimited-and-fine.
    ///
    /// Derived from [`resources`](Self::resources); a sensor declares the
    /// budget by carrying the block, not by overriding this.
    fn budget_bytes(&self) -> Option<u64> {
        self.resources().budget_bytes()
    }

    /// `@desired` reconcile settings (#816/#849) — the kill switch and the
    /// re-seed interval. Every sensor that adopts a fleet-authorable topic
    /// (`thresholds`, `expectations`, …) reads them from here, so the
    /// framework can spawn the reconciler without knowing the config type.
    /// Default: enabled, with the standard refresh.
    fn desired(&self) -> zensight_common::desired::DesiredConfig {
        zensight_common::desired::DesiredConfig::default()
    }

    /// Operator-authored threshold rules (`thresholds.*`, #928/#931).
    ///
    /// Default: **no rules**. That is not "thresholds are off" — the
    /// evaluator is installed either way, because `@desired` and
    /// `@rpc/<producer>/thresholds/set` can add rules to a running sensor.
    /// It is "this build ships no threshold that fires", which is the
    /// standing rule for every number in this tree that an operator did not
    /// choose.
    fn thresholds(&self) -> zensight_common::threshold::ThresholdsConfig {
        zensight_common::threshold::ThresholdsConfig::default()
    }

    /// Validate the configuration.
    ///
    /// Called automatically after loading. Override to add custom validation.
    fn validate(&self) -> Result<()> {
        Ok(())
    }

    /// Load configuration from a file path.
    ///
    /// Supports JSON5 format. Calls [`validate`](Self::validate) after loading.
    fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(SensorError::ConfigNotFound {
                path: path.display().to_string(),
            });
        }

        let content = std::fs::read_to_string(path)?;
        let config: Self = json5::from_str(&content)?;

        config.validate()?;

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        zenoh: ZenohConfig,
        logging: LoggingConfig,
    }

    impl SensorConfig for TestConfig {
        fn zenoh(&self) -> &ZenohConfig {
            &self.zenoh
        }

        fn logging(&self) -> &LoggingConfig {
            &self.logging
        }

        fn producer(&self) -> &str {
            "test"
        }
    }

    #[test]
    fn test_config_not_found() {
        let result = TestConfig::load("/nonexistent/path.json5");
        assert!(matches!(result, Err(SensorError::ConfigNotFound { .. })));
    }
}
