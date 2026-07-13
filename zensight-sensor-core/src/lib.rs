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
//!     let source = config.resolved_source();
//!
//!     let runner = SensorRunner::new("mysensor", source, config).await?;
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
pub mod cloud;
mod config;
pub mod container;
mod error;
mod health;
mod identity;
mod liveliness;
pub mod procutil;
mod publisher;
pub mod report;
pub mod rpc;
mod runner;
pub mod scrub;
pub mod v1;

pub use advanced_publisher::{AdvancedPublisherConfig, AdvancedPublisherRegistry};
pub use alert::{AlertReporter, serve_alerts_query};
pub use args::SensorArgs;
pub use artifact::{
    ArtifactChannel, ArtifactProducer, DeliveryKind, ProduceCtx, Produced, ProgressUpdate,
    ReportProducer, SnapshotProducer,
};
pub use cloud::detect_cloud;
pub use config::SensorConfig;
pub use container::{container_id_from_cgroup, container_id_from_path, detect_self_container_id};
pub use error::{Result, SensorError};
pub use health::{
    DeviceLiveness, DeviceStatus, ErrorReport, ErrorType, HealthSnapshot, SensorHealth,
};
pub use identity::{HostIdentity, SharedIdentity};
pub use liveliness::LivelinessManager;
pub use publisher::{Publisher, RawMediaPublisher};
pub use report::{DebugBundleSource, SimpleBundleSource, redact};
pub use runner::SensorRunner;
pub use scrub::{ArgScrubber, CMDLINE_CAP_BYTES};

// Re-export commonly used types from zensight-common
pub use zensight_common::{
    Alert, AlertKind, AlertSeverity, AlertState, ArtifactLimits, ArtifactReportLimits,
    ArtifactSnapshotLimits, CloudFacts, CommonArtifactLimits, Format, IdentityConfig, LogFormat,
    LoggingConfig, Protocol, SnapshotDir, TelemetryPoint, TelemetryValue, ZenohConfig,
};
