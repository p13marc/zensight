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
//! Messages are published to:
//! ```text
//! {prefix}/{hostname}/{facility}/{severity}
//! ```
//!
//! For example:
//! ```text
//! zensight/logs/router01/auth/warning
//! zensight/logs/webserver/daemon/info
//! ```

pub mod commands;
pub mod config;
pub mod events;
pub mod filter;
pub mod ingest;
#[cfg(feature = "journald")]
pub mod journald;
pub mod parser;
pub mod receiver;
