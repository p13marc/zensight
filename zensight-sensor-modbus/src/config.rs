//! Configuration for the Modbus sensor.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;
use zensight_common::config::ZenohConfig;

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_sensor_core::LoggingConfig;

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

/// Complete sensor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModbusSensorConfig {
    /// Zenoh connection settings
    pub zenoh: ZenohConfig,

    /// Modbus-specific settings
    pub modbus: ModbusConfig,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Declared resource envelope (#811/#1091). `resources.budget_rss_mb` is
    /// carried into the health doc's `self_stats.budget_bytes` and graded by
    /// the runner's `sensor-budget` rule at 80 % — declared, not enforced
    /// (#812 is the shed ladder). Absent reads as *undeclared*, never as
    /// unlimited-and-fine.
    #[serde(default)]
    pub resources: zensight_sensor_core::ResourcesConfig,

    /// On-demand artifact channel (`@rpc/modbus/artifact/*`) limits — report + snapshot.
    /// Every kind disabled by default.
    #[serde(default)]
    pub artifacts: zensight_sensor_core::ArtifactLimits,

    /// `@desired` reconcile settings (#931): the kill switch and refresh
    /// cadence. File config on purpose — the mechanism that could misbehave
    /// must be disarmable from outside itself.
    #[serde(default)]
    pub desired: zensight_common::desired::DesiredConfig,

    /// Operator-authored threshold rules over this sensor's own telemetry
    /// (#931). **Empty by default** — this build ships no threshold that
    /// fires. Also authorable fleet-wide on `@desired` and per-host over
    /// `@rpc/modbus/thresholds/set`; `state/modbus/applied/thresholds`
    /// says which of the three is in force.
    #[serde(default)]
    pub thresholds: zensight_common::threshold::ThresholdsConfig,
}

/// Modbus protocol configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModbusConfig {
    /// Override the agent-host source id (default: the local hostname).
    #[serde(default)]
    pub source: Option<String>,

    /// Devices to poll
    pub devices: Vec<DeviceConfig>,

    /// Named register groups (reusable across devices)
    #[serde(default)]
    pub register_groups: HashMap<String, RegisterGroup>,

    /// Register name mappings
    #[serde(default)]
    pub register_names: HashMap<String, String>,
}

impl ModbusConfig {
    /// The agent host's unified source id: the `source` override, else the hostname.
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string())
        })
    }
}

/// Configuration for a single Modbus device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Device name (used in key expressions)
    pub name: String,

    /// Connection type and address
    pub connection: ConnectionConfig,

    /// Modbus unit/slave ID (1-247)
    #[serde(default = "default_unit_id")]
    pub unit_id: u8,

    /// Poll interval in seconds
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Registers to poll (inline definition)
    #[serde(default)]
    pub registers: Vec<RegisterConfig>,

    /// Reference to a named register group
    #[serde(default)]
    pub register_group: Option<String>,

    /// Connection timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Retry count on failure
    #[serde(default = "default_retries")]
    pub retries: u32,
}

fn default_unit_id() -> u8 {
    1
}

fn default_poll_interval() -> u64 {
    10
}

fn default_timeout_ms() -> u64 {
    1000
}

fn default_retries() -> u32 {
    3
}

/// Connection configuration (TCP or RTU).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConnectionConfig {
    /// Modbus TCP connection
    Tcp {
        /// Host address (IP or hostname)
        host: String,
        /// TCP port (default: 502)
        #[serde(default = "default_modbus_port")]
        port: u16,
    },
    /// Modbus RTU (serial) connection
    Rtu {
        /// Serial port path (e.g., "/dev/ttyUSB0" or "COM1")
        port: String,
        /// Baud rate (default: 9600)
        #[serde(default = "default_baud_rate")]
        baud_rate: u32,
        /// Data bits (default: 8)
        #[serde(default = "default_data_bits")]
        data_bits: u8,
        /// Parity: "none", "even", or "odd" (default: "none")
        #[serde(default = "default_parity")]
        parity: String,
        /// Stop bits: 1 or 2 (default: 1)
        #[serde(default = "default_stop_bits")]
        stop_bits: u8,
    },
}

fn default_modbus_port() -> u16 {
    502
}

fn default_baud_rate() -> u32 {
    9600
}

fn default_data_bits() -> u8 {
    8
}

fn default_parity() -> String {
    "none".to_string()
}

fn default_stop_bits() -> u8 {
    1
}

/// A group of registers to poll together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterGroup {
    /// Registers in this group
    pub registers: Vec<RegisterConfig>,
}

/// Configuration for a register or range of registers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterConfig {
    /// Register type
    #[serde(rename = "type")]
    pub register_type: RegisterType,

    /// Starting address (0-based)
    pub address: u16,

    /// Number of registers to read (default: 1)
    #[serde(default = "default_count")]
    pub count: u16,

    /// Optional name for this register (used in key expression)
    pub name: Option<String>,

    /// Data type interpretation for holding/input registers
    #[serde(default)]
    pub data_type: DataType,

    /// Scaling factor (value * scale)
    #[serde(default = "default_scale")]
    pub scale: f64,

    /// Offset (value * scale + offset)
    #[serde(default)]
    pub offset: f64,

    /// Unit of measurement (for metadata)
    pub unit: Option<String>,
}

fn default_count() -> u16 {
    1
}

fn default_scale() -> f64 {
    1.0
}

/// Modbus register types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegisterType {
    /// Discrete output coils (read/write, 1-bit)
    Coil,
    /// Discrete input contacts (read-only, 1-bit)
    Discrete,
    /// Input registers (read-only, 16-bit)
    Input,
    /// Holding registers (read/write, 16-bit)
    Holding,
}

impl RegisterType {
    /// Return the string name for this register type.
    pub fn as_str(&self) -> &'static str {
        match self {
            RegisterType::Coil => "coil",
            RegisterType::Discrete => "discrete",
            RegisterType::Input => "input",
            RegisterType::Holding => "holding",
        }
    }
}

/// Data type interpretation for 16-bit registers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataType {
    /// Unsigned 16-bit integer (default)
    #[default]
    U16,
    /// Signed 16-bit integer
    I16,
    /// Unsigned 32-bit integer (2 registers, big-endian)
    U32,
    /// Signed 32-bit integer (2 registers, big-endian)
    I32,
    /// 32-bit float (2 registers, big-endian)
    F32,
    /// Unsigned 32-bit integer (2 registers, little-endian word order)
    U32Le,
    /// Signed 32-bit integer (2 registers, little-endian word order)
    I32Le,
    /// 32-bit float (2 registers, little-endian word order)
    F32Le,
}

impl ModbusSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: ModbusSensorConfig = json5::from_str(&content)?;
        config.validate_config()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate_config(&self) -> Result<(), ConfigError> {
        if self.modbus.devices.is_empty() {
            return Err(ConfigError::Validation(
                "At least one device must be configured".to_string(),
            ));
        }

        for device in &self.modbus.devices {
            if device.name.is_empty() {
                return Err(ConfigError::Validation(
                    "Device name cannot be empty".to_string(),
                ));
            }

            if device.unit_id == 0 {
                return Err(ConfigError::Validation(format!(
                    "Device '{}': unit_id must be 1-247",
                    device.name
                )));
            }

            // Check that device has either inline registers or a register group
            let has_registers = !device.registers.is_empty();
            let has_group = device.register_group.is_some();

            if !has_registers && !has_group {
                return Err(ConfigError::Validation(format!(
                    "Device '{}': must specify either registers or register_group",
                    device.name
                )));
            }

            // Validate register group reference
            if let Some(group_name) = &device.register_group
                && !self.modbus.register_groups.contains_key(group_name)
            {
                return Err(ConfigError::Validation(format!(
                    "Device '{}': unknown register_group '{}'",
                    device.name, group_name
                )));
            }

            // Every register block this device polls, whether inline or from a
            // named group.
            let group = device
                .register_group
                .as_ref()
                .and_then(|g| self.modbus.register_groups.get(g))
                .map(|g| g.registers.as_slice())
                .unwrap_or(&[]);
            for register in device.registers.iter().chain(group) {
                validate_register(&device.name, register)?;
            }

            // Validate RTU parity
            if let ConnectionConfig::Rtu { parity, .. } = &device.connection {
                match parity.to_lowercase().as_str() {
                    "none" | "even" | "odd" => {}
                    _ => {
                        return Err(ConfigError::Validation(format!(
                            "Device '{}': invalid parity '{}' (use none, even, or odd)",
                            device.name, parity
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

/// One register block's own consistency (#1073).
///
/// Both rules exist because a config that breaks them publishes plausible wrong
/// numbers rather than failing:
///
/// - A `name` names ONE value. With `count > 1` it was returned for every
///   decoded value, so `{name: "temperature", count: 10}` published ten sensors
///   to `…/holding/temperature`, ten times a cycle, and nine were lost —
///   silently, at whatever cadence the poll ran.
/// - A block whose span does not fit in `u16` cannot be addressed. The span
///   used to be an unchecked `count * regs_per_value`, which panicked in debug
///   and wrapped in release; a wrapped span reads as a short, legal read of the
///   wrong window.
fn validate_register(device: &str, register: &RegisterConfig) -> Result<(), ConfigError> {
    if register.name.is_some() && register.count > 1 {
        return Err(ConfigError::Validation(format!(
            "Device '{device}': register at {} has `name` and `count` = {} — a name names one \
             value, and returning it for all of them publishes {} readings to one key and keeps \
             the last. Give one entry per register, or drop `name` and use `register_names` \
             (\"{}:{}\": …), which is keyed by address",
            register.address,
            register.count,
            register.count,
            register.register_type.as_str(),
            register.address,
        )));
    }
    if register.count == 0 {
        return Err(ConfigError::Validation(format!(
            "Device '{device}': register at {} has `count` = 0 — a block that reads nothing is a \
             typo, not a configuration",
            register.address
        )));
    }
    let per_value: u16 = match register.data_type {
        DataType::U16 | DataType::I16 => 1,
        _ => 2,
    };
    let span = register.count.checked_mul(per_value).ok_or_else(|| {
        ConfigError::Validation(format!(
            "Device '{device}': register at {} spans {} × {per_value} registers, which exceeds \
             the 16-bit address space",
            register.address, register.count
        ))
    })?;
    // The last register the block touches must still be addressable.
    span.checked_sub(1)
        .and_then(|last| register.address.checked_add(last))
        .ok_or_else(|| ConfigError::Validation(format!(
            "Device '{device}': register block at {} spanning {span} registers runs past address \
             65535",
            register.address
        )))?;
    Ok(())
}

impl zensight_sensor_core::SensorConfig for ModbusSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &str {
        "modbus"
    }

    fn resources(&self) -> &zensight_sensor_core::ResourcesConfig {
        &self.resources
    }

    fn desired(&self) -> zensight_common::desired::DesiredConfig {
        self.desired.clone()
    }

    fn thresholds(&self) -> zensight_common::threshold::ThresholdsConfig {
        self.thresholds.clone()
    }

    fn artifact_limits(&self) -> zensight_sensor_core::ArtifactLimits {
        self.artifacts.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        self.validate_config()
            .map_err(|e| zensight_sensor_core::SensorError::config(e.to_string()))
    }
}

impl DeviceConfig {
    /// Get all registers for this device, including those from register groups.
    pub fn all_registers(&self, groups: &HashMap<String, RegisterGroup>) -> Vec<RegisterConfig> {
        let mut registers = self.registers.clone();

        if let Some(group_name) = &self.register_group
            && let Some(group) = groups.get(group_name)
        {
            registers.extend(group.registers.clone());
        }

        registers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tcp_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            modbus: {
                devices: [
                    {
                        name: "plc01",
                        connection: { type: "tcp", host: "192.168.1.10" },
                        registers: [
                            { type: "holding", address: 0, count: 10 }
                        ]
                    }
                ]
            }
        }"#;

        let config: ModbusSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.modbus.devices.len(), 1);
        assert_eq!(config.modbus.devices[0].name, "plc01");

        if let ConnectionConfig::Tcp { host, port } = &config.modbus.devices[0].connection {
            assert_eq!(host, "192.168.1.10");
            assert_eq!(*port, 502); // default
        } else {
            panic!("Expected TCP connection");
        }
    }

    #[test]
    fn test_parse_rtu_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            modbus: {
                devices: [
                    {
                        name: "sensor01",
                        connection: {
                            type: "rtu",
                            port: "/dev/ttyUSB0",
                            baud_rate: 19200,
                            parity: "even"
                        },
                        unit_id: 5,
                        registers: [
                            { type: "input", address: 0, count: 4, data_type: "f32" }
                        ]
                    }
                ]
            }
        }"#;

        let config: ModbusSensorConfig = json5::from_str(json).unwrap();
        let device = &config.modbus.devices[0];

        assert_eq!(device.unit_id, 5);
        if let ConnectionConfig::Rtu {
            port,
            baud_rate,
            parity,
            ..
        } = &device.connection
        {
            assert_eq!(port, "/dev/ttyUSB0");
            assert_eq!(*baud_rate, 19200);
            assert_eq!(parity, "even");
        } else {
            panic!("Expected RTU connection");
        }
    }

    #[test]
    fn test_register_groups() {
        let json = r#"{
            zenoh: { mode: "peer" },
            modbus: {
                devices: [
                    {
                        name: "plc01",
                        connection: { type: "tcp", host: "192.168.1.10" },
                        register_group: "power_meters"
                    }
                ],
                register_groups: {
                    power_meters: {
                        registers: [
                            { type: "holding", address: 0, count: 1, name: "voltage", data_type: "f32", unit: "V" },
                            { type: "holding", address: 2, count: 1, name: "current", data_type: "f32", unit: "A" }
                        ]
                    }
                }
            }
        }"#;

        let config: ModbusSensorConfig = json5::from_str(json).unwrap();
        config.validate_config().unwrap();

        let device = &config.modbus.devices[0];
        let registers = device.all_registers(&config.modbus.register_groups);
        assert_eq!(registers.len(), 2);
        assert_eq!(registers[0].name.as_deref(), Some("voltage"));
    }

    /// A `name` names ONE value (#1073). It used to be returned for every
    /// decoded value in the block, so `{name: "temperature", count: 10}`
    /// published ten sensors to `…/holding/temperature`, ten times a cycle,
    /// and nine were lost. `configs/modbus.json5` shipped in exactly that shape.
    ///
    /// Refused in a register GROUP as well as inline — the group is where the
    /// shipped example put it.
    #[test]
    fn a_name_with_more_than_one_value_is_refused() {
        let cfg = |registers: &str| {
            format!(
                r#"{{ zenoh: {{ mode: "peer" }}, modbus: {{ devices: [
                    {{ name: "plc01", connection: {{ type: "tcp", host: "10.0.0.1" }},
                       registers: [{registers}] }}
                ] }} }}"#
            )
        };
        let bad: ModbusSensorConfig = json5::from_str(&cfg(
            r#"{ type: "holding", address: 0, count: 10, name: "temperature", data_type: "u16" }"#,
        ))
        .unwrap();
        let err = bad.validate_config().unwrap_err().to_string();
        assert!(err.contains("`name` and `count`"), "{err}");
        assert!(
            err.contains("register_names"),
            "the refusal must name the alternative: {err}"
        );

        // One value with a name is fine, and so is many values without one.
        for ok in [
            r#"{ type: "holding", address: 0, count: 1, name: "temperature", data_type: "f32" }"#,
            r#"{ type: "holding", address: 0, count: 10, data_type: "u16" }"#,
        ] {
            let c: ModbusSensorConfig = json5::from_str(&cfg(ok)).unwrap();
            c.validate_config().expect(ok);
        }

        // And in a group.
        let grouped: ModbusSensorConfig = json5::from_str(
            r#"{ zenoh: { mode: "peer" }, modbus: {
                devices: [ { name: "plc01", connection: { type: "tcp", host: "10.0.0.1" },
                             register_group: "g" } ],
                register_groups: { g: { registers: [
                    { type: "holding", address: 0, count: 4, name: "v", data_type: "f32" }
                ] } } } }"#,
        )
        .unwrap();
        assert!(grouped.validate_config().is_err());
    }

    /// A block whose span does not fit the 16-bit address space is refused
    /// rather than wrapped. `count * regs_per_value` was an unchecked multiply:
    /// it panicked in debug and wrapped in release, and a wrapped span reads as
    /// a short, legal read of the wrong window (#1073).
    #[test]
    fn a_block_that_runs_past_the_address_space_is_refused() {
        let cfg = |address: u16, count: u16| {
            format!(
                r#"{{ zenoh: {{ mode: "peer" }}, modbus: {{ devices: [
                    {{ name: "plc01", connection: {{ type: "tcp", host: "10.0.0.1" }},
                       registers: [{{ type: "holding", address: {address}, count: {count},
                                     data_type: "f32" }}] }}
                ] }} }}"#
            )
        };
        // 40000 f32 values is 80000 registers — the multiply itself overflows.
        let c: ModbusSensorConfig = json5::from_str(&cfg(0, 40_000)).unwrap();
        assert!(c.validate_config().is_err());
        // And a block that fits in a u16 span but runs off the end of the map.
        let c: ModbusSensorConfig = json5::from_str(&cfg(65_530, 10)).unwrap();
        assert!(c.validate_config().is_err());
        // The largest block that does fit is accepted.
        let c: ModbusSensorConfig = json5::from_str(&cfg(65_534, 1)).unwrap();
        c.validate_config()
            .expect("a block ending exactly at 65535");
        // A count of zero is a typo, not a configuration.
        let c: ModbusSensorConfig = json5::from_str(&cfg(0, 0)).unwrap();
        assert!(c.validate_config().is_err());
    }

    #[test]
    fn test_validate_empty_devices() {
        let json = r#"{
            zenoh: { mode: "peer" },
            modbus: { devices: [] }
        }"#;

        let config: ModbusSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_validate_missing_registers() {
        let json = r#"{
            zenoh: { mode: "peer" },
            modbus: {
                devices: [
                    {
                        name: "plc01",
                        connection: { type: "tcp", host: "192.168.1.10" }
                    }
                ]
            }
        }"#;

        let config: ModbusSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate_config().is_err());
    }

    #[test]
    fn test_data_type_default() {
        let reg = RegisterConfig {
            register_type: RegisterType::Holding,
            address: 0,
            count: 1,
            name: None,
            data_type: DataType::default(),
            scale: 1.0,
            offset: 0.0,
            unit: None,
        };
        assert_eq!(reg.data_type, DataType::U16);
    }
}

/// The shipped example config must load (#845): it ships in the release
/// tarball and the container image, and nothing else in CI ever parsed it —
/// so a renamed field silently reverted to its serde default in production
/// (`gen-configs.sh` documents the identical hazard for the demo profile).
/// Precedent: parallax/logs guard their shipped configs the same way.
#[cfg(test)]
mod shipped_config {
    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/modbus.json5");
        let _config = crate::config::ModbusSensorConfig::load_from_file(path)
            .expect("configs/modbus.json5 must load");
    }
}
