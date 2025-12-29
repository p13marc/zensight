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
//! │  (zensight/**)  │     │  (aggregation)  │     │   (/metrics)    │
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

pub mod collector;
pub mod config;
pub mod http;
pub mod mapping;
pub mod subscriber;

pub use collector::{MetricCollector, SharedCollector};
pub use config::ExporterConfig;
pub use http::HttpServer;
pub use subscriber::TelemetrySubscriber;
