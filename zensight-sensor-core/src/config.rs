//! Configuration traits and utilities.

use std::path::Path;

use serde::de::DeserializeOwned;

use crate::error::{Result, SensorError};
use crate::{LoggingConfig, ZenohConfig};
use zensight_common::{ReportLimits, SnapshotLimits};

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
///     fn key_prefix(&self) -> &str {
///         &self.my_protocol.key_prefix
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

    /// Get the key expression prefix for this sensor.
    fn key_prefix(&self) -> &str;

    /// Debug-report limits/policy. Defaults to disabled; a sensor opts in by
    /// overriding this to return its configured [`ReportLimits`] (and enabling
    /// `with_report` in `main`).
    fn report_limits(&self) -> ReportLimits {
        ReportLimits::default()
    }

    /// Tier-2 directory-snapshot limits/policy. Defaults to disabled; a sensor
    /// opts in by overriding this to return its configured [`SnapshotLimits`] (and
    /// enabling `with_snapshot` in `main`).
    fn snapshot_limits(&self) -> SnapshotLimits {
        SnapshotLimits::default()
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
        key_prefix: String,
    }

    impl SensorConfig for TestConfig {
        fn zenoh(&self) -> &ZenohConfig {
            &self.zenoh
        }

        fn logging(&self) -> &LoggingConfig {
            &self.logging
        }

        fn key_prefix(&self) -> &str {
            &self.key_prefix
        }
    }

    #[test]
    fn test_config_not_found() {
        let result = TestConfig::load("/nonexistent/path.json5");
        assert!(matches!(result, Err(SensorError::ConfigNotFound { .. })));
    }
}
