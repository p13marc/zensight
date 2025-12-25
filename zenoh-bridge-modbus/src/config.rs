//! Configuration for the Modbus bridge.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;
use zensight_common::config::ZenohConfig;

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

/// Complete bridge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModbusBridgeConfig {
    /// Zenoh connection settings
    pub zenoh: ZenohConfig,

    /// Modbus-specific settings
    pub modbus: ModbusConfig,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Modbus protocol configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModbusConfig {
    /// Key expression prefix (default: "zensight/modbus")
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Devices to poll
    pub devices: Vec<DeviceConfig>,

    /// Named register groups (reusable across devices)
    #[serde(default)]
    pub register_groups: HashMap<String, RegisterGroup>,

    /// Register name mappings
    #[serde(default)]
    pub register_names: HashMap<String, String>,
}

fn default_key_prefix() -> String {
    "zensight/modbus".to_string()
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

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error"
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

impl ModbusBridgeConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: ModbusBridgeConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
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
            if let Some(group_name) = &device.register_group {
                if !self.modbus.register_groups.contains_key(group_name) {
                    return Err(ConfigError::Validation(format!(
                        "Device '{}': unknown register_group '{}'",
                        device.name, group_name
                    )));
                }
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

impl DeviceConfig {
    /// Get all registers for this device, including those from register groups.
    pub fn all_registers(&self, groups: &HashMap<String, RegisterGroup>) -> Vec<RegisterConfig> {
        let mut registers = self.registers.clone();

        if let Some(group_name) = &self.register_group {
            if let Some(group) = groups.get(group_name) {
                registers.extend(group.registers.clone());
            }
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

        let config: ModbusBridgeConfig = json5::from_str(json).unwrap();
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

        let config: ModbusBridgeConfig = json5::from_str(json).unwrap();
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
                            { type: "holding", address: 0, count: 2, name: "voltage", data_type: "f32", unit: "V" },
                            { type: "holding", address: 2, count: 2, name: "current", data_type: "f32", unit: "A" }
                        ]
                    }
                }
            }
        }"#;

        let config: ModbusBridgeConfig = json5::from_str(json).unwrap();
        config.validate().unwrap();

        let device = &config.modbus.devices[0];
        let registers = device.all_registers(&config.modbus.register_groups);
        assert_eq!(registers.len(), 2);
        assert_eq!(registers[0].name.as_deref(), Some("voltage"));
    }

    #[test]
    fn test_validate_empty_devices() {
        let json = r#"{
            zenoh: { mode: "peer" },
            modbus: { devices: [] }
        }"#;

        let config: ModbusBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
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

        let config: ModbusBridgeConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
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
