//! Zenoh sensor for Syslog telemetry.
//!
//! This crate provides a syslog receiver that publishes messages to Zenoh.
//!
//! # Supported Formats
//!
//! - RFC 3164 (BSD syslog)
//! - RFC 5424 (structured syslog)
//!
//! # Key Expression Format
//!
//! Per-line messages feed the bounded `@rpc/logs/events` ring (#358, pull-only);
//! the derived rollups ride the telemetry bus under:
//! ```text
//! zensight/v1/<origin>/telemetry/logs/...
//! ```

pub mod commands;
pub mod config;
pub mod events;
pub mod filter;
pub mod ingest;
#[cfg(feature = "journald")]
pub mod journald;
pub mod multiline;
pub mod parser;
/// The bounded per-line event ring + `@rpc/logs/events` queryable. Public so
/// the integration harness (#548) can drive socket→ring→query round-trips.
pub mod query;
pub mod receiver;
pub(crate) mod telemetry_guard;
