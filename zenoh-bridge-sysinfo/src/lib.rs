//! Zenoh bridge for system monitoring.
//!
//! This bridge collects local system metrics (CPU, memory, disk, network)
//! using the `sysinfo` crate and publishes them to Zenoh as telemetry.
//!
//! # Key Expressions
//!
//! ```text
//! zensight/sysinfo/<hostname>/cpu/usage
//! zensight/sysinfo/<hostname>/cpu/<core_id>/usage
//! zensight/sysinfo/<hostname>/memory/used
//! zensight/sysinfo/<hostname>/memory/available
//! zensight/sysinfo/<hostname>/disk/<mount>/usage
//! zensight/sysinfo/<hostname>/network/<interface>/rx_bytes
//! ```

pub mod collector;
pub mod config;
