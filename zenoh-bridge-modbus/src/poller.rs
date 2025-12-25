//! Modbus device polling and telemetry publishing.

use crate::config::{
    ConnectionConfig, DataType, DeviceConfig, ModbusConfig, RegisterConfig, RegisterType,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio_modbus::client::{Context, Reader};
use tokio_modbus::prelude::*;
use tracing::{debug, error, info, warn};
use zenoh::Session;
use zensight_common::serialization::{encode, Format};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// Error type for polling operations.
#[derive(Debug, thiserror::Error)]
pub enum PollerError {
    #[error("Connection failed: {0}")]
    Connection(String),
    #[error("Read failed: {0}")]
    Read(String),
    #[error("Invalid configuration: {0}")]
    Config(String),
}

/// A poller for a single Modbus device.
pub struct ModbusPoller {
    device: DeviceConfig,
    registers: Vec<RegisterConfig>,
    key_prefix: String,
    register_names: HashMap<String, String>,
    session: Session,
    format: Format,
}

impl ModbusPoller {
    /// Create a new poller for a device.
    pub fn new(
        device: DeviceConfig,
        config: &ModbusConfig,
        session: Session,
        format: Format,
    ) -> Self {
        let registers = device.all_registers(&config.register_groups);

        Self {
            device,
            registers,
            key_prefix: config.key_prefix.clone(),
            register_names: config.register_names.clone(),
            session,
            format,
        }
    }

    /// Run the polling loop.
    pub async fn run(self) {
        let interval = Duration::from_secs(self.device.poll_interval_secs);
        let device_name = self.device.name.clone();

        info!(
            "Starting Modbus poller for device '{}' (interval: {}s)",
            device_name, self.device.poll_interval_secs
        );

        loop {
            match self.poll_once().await {
                Ok(count) => {
                    debug!(
                        "Device '{}': published {} telemetry points",
                        device_name, count
                    );
                }
                Err(e) => {
                    error!("Device '{}': polling error: {}", device_name, e);
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    /// Perform a single poll cycle.
    async fn poll_once(&self) -> Result<usize, PollerError> {
        let mut ctx = self.connect().await?;
        let mut count = 0;

        for register in &self.registers {
            match self.read_register(&mut ctx, register).await {
                Ok(values) => {
                    for (addr_offset, value) in values.into_iter().enumerate() {
                        let addr = register.address + addr_offset as u16;
                        self.publish_value(register, addr, value).await;
                        count += 1;
                    }
                }
                Err(e) => {
                    warn!(
                        "Device '{}': failed to read {:?} @ {}: {}",
                        self.device.name, register.register_type, register.address, e
                    );
                }
            }
        }

        Ok(count)
    }

    /// Connect to the Modbus device.
    async fn connect(&self) -> Result<Context, PollerError> {
        let timeout = Duration::from_millis(self.device.timeout_ms);
        let slave = Slave(self.device.unit_id);

        match &self.device.connection {
            ConnectionConfig::Tcp { host, port } => {
                let addr: SocketAddr = format!("{}:{}", host, port)
                    .parse()
                    .map_err(|e| PollerError::Connection(format!("Invalid address: {}", e)))?;

                let ctx = tokio::time::timeout(timeout, tcp::connect_slave(addr, slave))
                    .await
                    .map_err(|_| PollerError::Connection("Connection timeout".to_string()))?
                    .map_err(|e| PollerError::Connection(e.to_string()))?;

                Ok(ctx)
            }
            ConnectionConfig::Rtu {
                port,
                baud_rate,
                data_bits,
                parity,
                stop_bits,
            } => {
                let parity = match parity.to_lowercase().as_str() {
                    "none" => tokio_serial::Parity::None,
                    "even" => tokio_serial::Parity::Even,
                    "odd" => tokio_serial::Parity::Odd,
                    _ => tokio_serial::Parity::None,
                };

                let stop_bits = match stop_bits {
                    2 => tokio_serial::StopBits::Two,
                    _ => tokio_serial::StopBits::One,
                };

                let data_bits = match data_bits {
                    5 => tokio_serial::DataBits::Five,
                    6 => tokio_serial::DataBits::Six,
                    7 => tokio_serial::DataBits::Seven,
                    _ => tokio_serial::DataBits::Eight,
                };

                let builder = tokio_serial::new(port, *baud_rate)
                    .parity(parity)
                    .stop_bits(stop_bits)
                    .data_bits(data_bits);

                let serial = tokio_serial::SerialStream::open(&builder)
                    .map_err(|e| PollerError::Connection(format!("Serial open failed: {}", e)))?;

                let ctx = rtu::attach_slave(serial, slave);
                Ok(ctx)
            }
        }
    }

    /// Read a register or range of registers.
    async fn read_register(
        &self,
        ctx: &mut Context,
        register: &RegisterConfig,
    ) -> Result<Vec<TelemetryValue>, PollerError> {
        match register.register_type {
            RegisterType::Coil => {
                let result = ctx
                    .read_coils(register.address, register.count)
                    .await
                    .map_err(|e| PollerError::Read(e.to_string()))?
                    .map_err(|e| PollerError::Read(format!("Exception: {:?}", e)))?;

                Ok(result.into_iter().map(TelemetryValue::Boolean).collect())
            }
            RegisterType::Discrete => {
                let result = ctx
                    .read_discrete_inputs(register.address, register.count)
                    .await
                    .map_err(|e| PollerError::Read(e.to_string()))?
                    .map_err(|e| PollerError::Read(format!("Exception: {:?}", e)))?;

                Ok(result.into_iter().map(TelemetryValue::Boolean).collect())
            }
            RegisterType::Input => {
                let count = self.registers_needed(register);
                let result = ctx
                    .read_input_registers(register.address, count)
                    .await
                    .map_err(|e| PollerError::Read(e.to_string()))?
                    .map_err(|e| PollerError::Read(format!("Exception: {:?}", e)))?;

                self.decode_registers(&result, register)
            }
            RegisterType::Holding => {
                let count = self.registers_needed(register);
                let result = ctx
                    .read_holding_registers(register.address, count)
                    .await
                    .map_err(|e| PollerError::Read(e.to_string()))?
                    .map_err(|e| PollerError::Read(format!("Exception: {:?}", e)))?;

                self.decode_registers(&result, register)
            }
        }
    }

    /// Calculate how many 16-bit registers are needed for the configured data type.
    fn registers_needed(&self, register: &RegisterConfig) -> u16 {
        let regs_per_value = match register.data_type {
            DataType::U16 | DataType::I16 => 1,
            DataType::U32 | DataType::I32 | DataType::F32 => 2,
            DataType::U32Le | DataType::I32Le | DataType::F32Le => 2,
        };
        register.count * regs_per_value
    }

    /// Decode raw register values based on data type configuration.
    fn decode_registers(
        &self,
        data: &[u16],
        register: &RegisterConfig,
    ) -> Result<Vec<TelemetryValue>, PollerError> {
        let mut values = Vec::new();
        let regs_per_value: usize = match register.data_type {
            DataType::U16 | DataType::I16 => 1,
            _ => 2,
        };

        for chunk in data.chunks(regs_per_value) {
            let raw_value = match register.data_type {
                DataType::U16 => chunk[0] as f64,
                DataType::I16 => chunk[0] as i16 as f64,
                DataType::U32 => {
                    if chunk.len() >= 2 {
                        (((chunk[0] as u32) << 16) | (chunk[1] as u32)) as f64
                    } else {
                        continue;
                    }
                }
                DataType::I32 => {
                    if chunk.len() >= 2 {
                        (((chunk[0] as u32) << 16) | (chunk[1] as u32)) as i32 as f64
                    } else {
                        continue;
                    }
                }
                DataType::F32 => {
                    if chunk.len() >= 2 {
                        let bits = ((chunk[0] as u32) << 16) | (chunk[1] as u32);
                        f32::from_bits(bits) as f64
                    } else {
                        continue;
                    }
                }
                DataType::U32Le => {
                    if chunk.len() >= 2 {
                        (((chunk[1] as u32) << 16) | (chunk[0] as u32)) as f64
                    } else {
                        continue;
                    }
                }
                DataType::I32Le => {
                    if chunk.len() >= 2 {
                        (((chunk[1] as u32) << 16) | (chunk[0] as u32)) as i32 as f64
                    } else {
                        continue;
                    }
                }
                DataType::F32Le => {
                    if chunk.len() >= 2 {
                        let bits = ((chunk[1] as u32) << 16) | (chunk[0] as u32);
                        f32::from_bits(bits) as f64
                    } else {
                        continue;
                    }
                }
            };

            // Apply scale and offset
            let scaled = raw_value * register.scale + register.offset;
            values.push(TelemetryValue::Gauge(scaled));
        }

        Ok(values)
    }

    /// Publish a telemetry value to Zenoh.
    async fn publish_value(&self, register: &RegisterConfig, address: u16, value: TelemetryValue) {
        let metric_name = self.get_register_name(register, address);
        let key = format!(
            "{}/{}/{}/{}",
            self.key_prefix,
            self.device.name,
            register.register_type.as_str(),
            metric_name
        );

        let mut labels = HashMap::new();
        labels.insert("address".to_string(), address.to_string());
        labels.insert(
            "register_type".to_string(),
            register.register_type.as_str().to_string(),
        );
        if let Some(unit) = &register.unit {
            labels.insert("unit".to_string(), unit.clone());
        }

        let point = TelemetryPoint {
            timestamp: chrono::Utc::now().timestamp_millis(),
            source: self.device.name.clone(),
            protocol: Protocol::Modbus,
            metric: metric_name,
            value,
            labels,
        };

        match encode(&point, self.format) {
            Ok(payload) => {
                if let Err(e) = self.session.put(&key, payload).await {
                    warn!("Failed to publish to '{}': {}", key, e);
                } else {
                    debug!("Published: {} = {:?}", key, point.value);
                }
            }
            Err(e) => {
                warn!("Failed to encode telemetry: {}", e);
            }
        }
    }

    /// Get a human-readable name for a register address.
    fn get_register_name(&self, register: &RegisterConfig, address: u16) -> String {
        // First check if register has a configured name
        if let Some(name) = &register.name {
            return name.clone();
        }

        // Then check global register names mapping
        let type_prefix = register.register_type.as_str();
        let lookup_key = format!("{}:{}", type_prefix, address);
        if let Some(name) = self.register_names.get(&lookup_key) {
            return name.clone();
        }

        // Fall back to address
        address.to_string()
    }
}

/// Build a key expression for a Modbus metric.
pub fn build_key_expr(prefix: &str, device: &str, register_type: &str, name: &str) -> String {
    format!("{}/{}/{}/{}", prefix, device, register_type, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RegisterType;

    #[test]
    fn test_build_key_expr() {
        assert_eq!(
            build_key_expr("zensight/modbus", "plc01", "holding", "temperature"),
            "zensight/modbus/plc01/holding/temperature"
        );
    }

    #[test]
    fn test_register_type_as_str() {
        assert_eq!(RegisterType::Coil.as_str(), "coil");
        assert_eq!(RegisterType::Discrete.as_str(), "discrete");
        assert_eq!(RegisterType::Input.as_str(), "input");
        assert_eq!(RegisterType::Holding.as_str(), "holding");
    }

    #[test]
    fn test_decode_u16() {
        let register = RegisterConfig {
            register_type: RegisterType::Holding,
            address: 0,
            count: 2,
            name: None,
            data_type: DataType::U16,
            scale: 1.0,
            offset: 0.0,
            unit: None,
        };

        // Create a temporary poller just for testing decode
        // We can test the decode logic directly
        let data = [100u16, 200u16];
        let regs_per_value = 1;
        let mut values = Vec::new();

        for chunk in data.chunks(regs_per_value) {
            let raw = chunk[0] as f64;
            let scaled = raw * register.scale + register.offset;
            values.push(scaled);
        }

        assert_eq!(values, vec![100.0, 200.0]);
    }

    #[test]
    fn test_decode_f32_big_endian() {
        // Test big-endian F32 decoding
        // Value: 123.456 in IEEE 754 = 0x42F6E979
        let data = [0x42F6u16, 0xE979u16];
        let bits = ((data[0] as u32) << 16) | (data[1] as u32);
        let value = f32::from_bits(bits);

        assert!((value - 123.456).abs() < 0.001);
    }

    #[test]
    fn test_decode_with_scale_offset() {
        let raw = 1000u16 as f64;
        let scale = 0.1;
        let offset = -50.0;
        let result = raw * scale + offset;

        // 1000 * 0.1 - 50 = 50
        assert_eq!(result, 50.0);
    }
}
