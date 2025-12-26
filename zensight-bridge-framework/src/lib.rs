//! ZenSight Bridge Framework
//!
//! Common abstractions for building protocol bridges that publish telemetry to Zenoh.
//!
//! # Overview
//!
//! This framework provides:
//! - [`BridgeConfig`] trait for configuration loading and validation
//! - [`BridgeRunner`] for managing bridge lifecycle (startup, shutdown, signal handling)
//! - [`Publisher`] for publishing telemetry to Zenoh with automatic serialization
//! - [`BridgeArgs`] for common CLI argument parsing
//! - [`BridgeStatus`] for standardized status reporting
//!
//! # Example
//!
//! ```ignore
//! use zensight_bridge_framework::{BridgeArgs, BridgeConfig, BridgeRunner};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let args = BridgeArgs::parse("mybridge.json5");
//!     let config = MyBridgeConfig::load(&args.config)?;
//!
//!     let runner = BridgeRunner::new("mybridge", config).await?;
//!
//!     // Spawn protocol-specific workers
//!     runner.spawn(my_worker(runner.publisher()));
//!
//!     // Run until Ctrl+C
//!     runner.run().await
//! }
//! ```

mod advanced_publisher;
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
pub use args::BridgeArgs;
pub use config::BridgeConfig;
pub use correlation::{BridgeInfo, CorrelationEntry, CorrelationRegistry, DeviceIdentity};
pub use error::{BridgeError, Result};
pub use health::{
    BridgeHealth, DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot,
};
pub use liveliness::LivelinessManager;
pub use publisher::Publisher;
pub use runner::BridgeRunner;
pub use status::BridgeStatus;

// Re-export commonly used types from zensight-common
pub use zensight_common::{
    Format, LogFormat, LoggingConfig, Protocol, TelemetryPoint, TelemetryValue, ZenohConfig,
};
