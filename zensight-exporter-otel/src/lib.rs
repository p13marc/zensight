//! OpenTelemetry exporter for ZenSight telemetry.
//!
//! This crate provides an OpenTelemetry exporter that subscribes to ZenSight telemetry
//! over Zenoh and exports metrics and logs via OTLP (gRPC or HTTP).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
//! │  Zenoh Network  │────>│  OTEL Exporter  │────>│  OTLP Endpoint  │
//! │  (zensight/**)  │     │  (metrics/logs) │     │  (Collector)    │
//! └─────────────────┘     └─────────────────┘     └─────────────────┘
//! ```
//!
//! # Supported Signals
//!
//! - **Metrics**: Counter and Gauge telemetry values are exported as OTEL metrics
//! - **Logs**: Syslog text messages are exported as OTEL log records
//!
//! # Usage
//!
//! Run the exporter binary with a configuration file:
//!
//! ```bash
//! zensight-exporter-otel --config config.json5
//! ```
//!
//! # Configuration
//!
//! See [`config::ExporterConfig`] for configuration options.

pub mod config;
pub mod exporter;
pub mod logs;
pub mod metrics;
pub mod subscriber;

pub use config::ExporterConfig;
pub use exporter::{OtelExporter, SharedExporter};
pub use subscriber::TelemetrySubscriber;
