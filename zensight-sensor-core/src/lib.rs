//! ZenSight Sensor Framework
//!
//! Common abstractions for building protocol sensors that publish telemetry to Zenoh.
//!
//! # Overview
//!
//! This framework provides:
//! - [`SensorConfig`] trait for configuration loading and validation
//! - [`SensorRunner`] for managing sensor lifecycle (startup, shutdown, signal handling)
//! - [`Publisher`] for publishing telemetry to Zenoh with automatic serialization
//! - [`SensorArgs`] for common CLI argument parsing
//! - [`SensorStatus`] for standardized status reporting
//!
//! # Example
//!
//! ```ignore
//! use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let args = SensorArgs::parse("mysensor.json5");
//!     let config = MySensorConfig::load(&args.config)?;
//!
//!     let runner = SensorRunner::new("mysensor", config).await?;
//!
//!     // Spawn protocol-specific workers
//!     runner.spawn(my_worker(runner.publisher()));
//!
//!     // Run until Ctrl+C
//!     runner.run().await
//! }
//! ```

mod advanced_publisher;
mod alert;
mod args;
mod config;
mod correlation;
mod error;
mod health;
mod liveliness;
mod publisher;
mod runner;
mod status;

pub use advanced_publisher::{AdvancedPublisherConfig, AdvancedPublisherRegistry};
pub use alert::AlertReporter;
pub use args::SensorArgs;
pub use config::SensorConfig;
pub use correlation::{CorrelationEntry, CorrelationRegistry, DeviceIdentity, SensorInfo};
pub use error::{Result, SensorError};
pub use health::{
    DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot, SensorHealth,
};
pub use liveliness::LivelinessManager;
pub use publisher::Publisher;
pub use runner::SensorRunner;
pub use status::SensorStatus;

// Re-export commonly used types from zensight-common
pub use zensight_common::{
    Alert, AlertKind, AlertSeverity, AlertState, Format, LogFormat, LoggingConfig, Protocol,
    TelemetryPoint, TelemetryValue, ZenohConfig,
};
