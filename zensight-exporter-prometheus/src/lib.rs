//! Prometheus metrics exporter for ZenSight telemetry.
//!
//! This crate provides a Prometheus exporter that subscribes to ZenSight telemetry
//! over Zenoh and exposes metrics via an HTTP `/metrics` endpoint.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
//! │  Zenoh Network  │────>│    Collector    │────>│   HTTP Server   │
//! │  (v1 telemetry)  │     │  (aggregation)  │     │   (/metrics)    │
//! └─────────────────┘     └─────────────────┘     └─────────────────┘
//! ```
//!
//! # Usage
//!
//! Run the exporter binary with a configuration file:
//!
//! ```bash
//! zensight-exporter-prometheus --config config.json5
//! ```
//!
//! # Configuration
//!
//! See [`config::ExporterConfig`] for configuration options.

pub mod alerts;
pub mod collector;
pub mod config;
pub mod http;
pub mod mapping;
pub mod remote_write;
pub mod subscriber;

pub use collector::{MetricCollector, SharedCollector};
pub use config::ExporterConfig;
pub use http::HttpServer;
pub use remote_write::RemoteWriteClient;
pub use subscriber::TelemetrySubscriber;

/// The producer chunk this exporter's own framework documents ride under
/// (#1202): `v1/<origin>/state/exporter-prometheus/{health,errors,sensor,…}`.
/// It publishes no telemetry (RFC 04 §1.1); this names the process, not what
/// it forwards.
pub const PRODUCER: &str = "exporter-prometheus";
