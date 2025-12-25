//! Zenoh bridge for Modbus protocol.
//!
//! This bridge polls Modbus devices (TCP or RTU/serial) and publishes
//! register values to Zenoh as telemetry.
//!
//! # Key Expressions
//!
//! ```text
//! zensight/modbus/<device>/<register_type>/<address>
//! ```
//!
//! Where:
//! - `<device>` - Device name from configuration
//! - `<register_type>` - `coil`, `discrete`, `input`, or `holding`
//! - `<address>` - Register address (or name if configured)

pub mod config;
pub mod poller;
