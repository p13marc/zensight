//! Configuration traits and utilities.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Result, SensorError};
use crate::{LoggingConfig, ZenohConfig};
use zensight_common::ArtifactLimits;
use zensight_common::Format;

/// The declared resource envelope (#811/#1091).
///
/// One shape for every producer. Before #1091 this struct existed twice — as
/// `ResourcesConfig` in the netring sensor and `ResourceConfig` (singular) in
/// the historian, at two different nesting depths — and the other fourteen
/// sensors had no field at all, so `docs/ops/SIZING.md`'s instruction to "set
/// all three" named a key that nine of eleven shipped units could not accept.
///
/// The omission this block closes was silent because nothing checked for
/// unknown keys: a `resources` block in a config the binary did not know about
/// parsed clean and was discarded. Every config loads through
/// [`SensorConfig::parse_strict`] now (#1150), so the same mistake is a startup
/// refusal naming the key.
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
/// The `source` a producer publishes under: the operator's, else this host's
/// name (#1156).
///
/// # Sixteen copies, disagreeing five ways
///
/// Every sensor had its own. They were not the same function:
///
/// | | copies |
/// |---|---|
/// | rejects a **configured** empty string | 2 of 16 (systemd, hostspec) |
/// | rejects an empty **hostname** | 2 of 16 (bmc, pve) |
/// | non-UTF-8 hostname → `"unknown"` | 10 |
/// | non-UTF-8 hostname → lossy string | 5 |
/// | reads `source == "auto"` as unset | 4 (the rest use `Option`) |
///
/// Two of those are correctness, not taste:
///
/// - **A configured `source: ""` was honoured** by fourteen of them. An empty
///   source is not a name — it reaches the device identity, the evidence
///   documents and every alert label, and an empty chunk is not even a legal
///   key chunk (RFC 03 §1.5).
/// - **`into_string().ok()` maps every non-UTF-8 hostname to `"unknown"`**, so
///   two hosts whose names are not valid UTF-8 land on *the same identity* and
///   merge into one device. That is the same failure #1153 fixed for mount
///   points, one layer up. `to_string_lossy` keeps them distinct, which is why
///   it is the behaviour kept here: a mangled name that is still this host's
///   is strictly better than a tidy name shared with a stranger.
///
/// `"unknown"` remains only for the case where there is genuinely nothing to
/// say — no configured source and no readable hostname at all.
#[must_use]
pub fn resolved_source(configured: Option<&str>) -> String {
    if let Some(s) = configured
        && !s.is_empty()
        && s != "auto"
    {
        return s.to_string();
    }
    hostname::get()
        .ok()
        .map(|h| h.to_string_lossy().into_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub trait SensorConfig: Sized + DeserializeOwned {
    /// Get the Zenoh configuration.
    fn zenoh(&self) -> &ZenohConfig;

    /// Get the logging configuration.
    fn logging(&self) -> &LoggingConfig;

    /// The wire format this producer's framework documents use (#1155).
    ///
    /// Defaults to [`Format::default`], which is **CBOR** — and that agreement
    /// is the point. `SensorRunner` used to hard-code `Format::Json` here with
    /// the comment "Default to JSON, can be overridden", while
    /// `Format::default()` has been CBOR since the wire was made
    /// bytes-sensitive. Two defaults disagreeing meant every sensor had to
    /// remember `.with_format(config.serialization)`, and **five did not** —
    /// gnmi, logs, modbus, netflow and snmp all read `config.serialization`
    /// for their own publishers and left the runner's on JSON, so their
    /// health, registration and evidence documents went out in a format the
    /// deployment had not asked for. Nothing broke, because every consumer
    /// sniffs; the bandwidth CBOR exists to save simply was not saved.
    ///
    /// Override it when the config carries the operator's choice:
    ///
    /// ```ignore
    /// fn serialization(&self) -> Format {
    ///     self.serialization
    /// }
    /// ```
    fn serialization(&self) -> Format {
        Format::default()
    }

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
    /// Supports JSON5 format. Rejects unknown keys through
    /// [`parse_strict`](Self::parse_strict), then calls
    /// [`validate`](Self::validate).
    fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Err(SensorError::ConfigNotFound {
                path: path.display().to_string(),
            });
        }

        let content = std::fs::read_to_string(path)?;
        Self::parse_strict(&content)
    }

    /// Parse a JSON5 config, **refusing a key no struct declares** (#1150),
    /// then validate it.
    ///
    /// A misspelled key used to parse clean and take the Rust default on a
    /// production host. `zensight-sensor-sysinfo`'s config records what that
    /// costs: `temperatures` and `power` stayed dark for months because a key
    /// in the wrong block is indistinguishable from a key nobody wrote.
    ///
    /// **This is `serde_ignored`, not `serde(deny_unknown_fields)`**, and the
    /// difference is the point. `deny_unknown_fields` errors at the first
    /// struct that sees a stray key, so the message names the field but not
    /// where it sits; `serde_ignored` collects every unknown key by its **full
    /// dotted path** in one pass, so `snmp.devices.0.comunity` reads as itself.
    /// The two cannot be combined — `deny_unknown_fields` aborts before the
    /// collector runs. This mechanism is `zensight-sensor-logs`' (#547), lifted
    /// here so every producer gets it rather than one.
    ///
    /// Two exemptions, both deliberate:
    ///
    /// - **the `zenoh` block**, which stays forward-compatible: it is the one
    ///   block a newer participant must be able to hand to an older one during
    ///   a rollout;
    /// - **`allow_unknown_fields: true`**, the escape hatch for a mixed-version
    ///   fleet sharing one file. It downgrades the refusal to one `warn!`
    ///   naming the keys. Read off the raw tree, so it needs no field on any
    ///   config struct.
    fn parse_strict(content: &str) -> Result<Self> {
        let value: serde_json::Value = json5::from_str(content)?;
        let allow = value
            .get("allow_unknown_fields")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let mut unknown: Vec<String> = Vec::new();
        let config: Self = serde_ignored::deserialize(value, |path| {
            let path = path.to_string();
            if path == "allow_unknown_fields" || path == "zenoh" || path.starts_with("zenoh.") {
                return;
            }
            unknown.push(path);
        })
        .map_err(|e| SensorError::config(e.to_string()))?;

        if !unknown.is_empty() {
            let list = unknown.join(", ");
            if allow {
                tracing::warn!(
                    unknown_keys = %list,
                    "config has unknown keys (allow_unknown_fields is set — ignoring)"
                );
            } else {
                return Err(SensorError::config(format!(
                    "unknown config key(s): {list}. Fix the typo, or set \
                     allow_unknown_fields: true to ignore (mixed-version fleets)."
                )));
            }
        }

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

#[cfg(test)]
mod resolved_source_tests {
    use super::resolved_source;

    /// The operator's choice wins, verbatim.
    #[test]
    fn a_configured_source_is_used_as_written() {
        assert_eq!(resolved_source(Some("edge-01")), "edge-01");
    }

    /// #1156: `source: ""` was honoured by fourteen of the sixteen copies.
    /// An empty source reaches the device identity, the evidence documents and
    /// every alert label — and is not a legal key chunk (RFC 03 §1.5).
    #[test]
    fn a_configured_empty_source_falls_back_to_the_hostname() {
        let empty = resolved_source(Some(""));
        assert!(!empty.is_empty(), "an empty source is never published");
        assert_eq!(empty, resolved_source(None), "it falls back like `None`");
    }

    /// Four crates spelled "unset" as the literal `"auto"` and the rest as
    /// `None`; both reach the same place now.
    #[test]
    fn the_auto_sentinel_means_unset() {
        assert_eq!(resolved_source(Some("auto")), resolved_source(None));
    }

    /// Whatever the host is called, the answer is a usable name.
    #[test]
    fn the_fallback_is_never_empty() {
        let s = resolved_source(None);
        assert!(!s.is_empty());
    }
}
