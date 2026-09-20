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
//! │  (v1 telemetry)  │     │  (metrics/logs) │     │  (Collector)    │
//! └─────────────────┘     └─────────────────┘     └─────────────────┘
//! ```
//!
//! # Supported Signals
//!
//! - **Metrics**: Counter and Gauge telemetry values are exported as OTEL metrics
//! - **Logs**: Syslog text messages are exported as OTEL log records
//! - **Alerts**: sensor alerts (`state/*/alert/*`) are exported as OTEL log events on
//!   the `zensight.alerts` scope (severity-mapped, with `alert.*` attributes)
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
pub mod traces;

pub use config::ExporterConfig;
pub use exporter::{OtelExporter, SharedExporter};
pub use subscriber::TelemetrySubscriber;

/// The producer chunk this exporter's own framework documents ride under
/// (#1202): `v1/<origin>/state/exporter-otel/{health,errors,sensor,…}`. It
/// publishes no telemetry (RFC 04 §1.1); this names the process, not what it
/// forwards.
pub const PRODUCER: &str = "exporter-otel";
