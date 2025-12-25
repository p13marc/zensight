//! Zenoh bridge for Syslog telemetry.
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
//! zensight/syslog/router01/auth/warning
//! zensight/syslog/webserver/daemon/info
//! ```

pub mod config;
pub mod parser;
pub mod receiver;
