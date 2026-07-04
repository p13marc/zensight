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
pub mod artifact;
mod config;
mod error;
mod health;
mod identity;
mod liveliness;
mod publisher;
pub mod report;
mod runner;
pub mod scrub;
mod status;

pub use advanced_publisher::{AdvancedPublisherConfig, AdvancedPublisherRegistry};
pub use alert::{AlertReporter, serve_alerts_query};
pub use args::SensorArgs;
pub use artifact::{
    ArtifactChannel, ArtifactProducer, DeliveryKind, ProduceCtx, Produced, ProgressUpdate,
    ReportProducer, SnapshotProducer,
};
pub use config::SensorConfig;
pub use error::{Result, SensorError};
pub use health::{
    DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot, SensorHealth,
};
pub use identity::{HostIdentity, SharedIdentity};
pub use liveliness::LivelinessManager;
pub use publisher::Publisher;
pub use report::{DebugBundleSource, SimpleBundleSource, redact};
pub use runner::SensorRunner;
pub use scrub::{ArgScrubber, CMDLINE_CAP_BYTES};
pub use status::SensorStatus;

// Re-export commonly used types from zensight-common
pub use zensight_common::{
    Alert, AlertKind, AlertSeverity, AlertState, ArtifactLimits, ArtifactReportLimits,
    ArtifactSnapshotLimits, CommonArtifactLimits, Format, LogFormat, LoggingConfig, Protocol,
    SnapshotDir, TelemetryPoint, TelemetryValue, ZenohConfig,
};
