//! Configuration traits and utilities.

use std::path::Path;

use serde::de::DeserializeOwned;

use crate::error::{Result, SensorError};
use crate::{LoggingConfig, ZenohConfig};
use zensight_common::ArtifactLimits;

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

    /// Declared memory budget in bytes (#811) — carried into the health
    /// doc's `self_stats.budget_bytes` and graded by the runner's
    /// `sensor-budget` rule at 80 %. **Declared, not enforced** (#812 is the
    /// enforcement ladder). Default: no budget — absent reads as
    /// *undeclared*, never as unlimited-and-fine. A sensor opts in by
    /// carrying e.g. `resources.budget_bytes` in its config and overriding
    /// this.
    fn budget_bytes(&self) -> Option<u64> {
        None
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
