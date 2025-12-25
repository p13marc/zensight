//! Zenoh bridge for NetFlow/IPFIX telemetry.
//!
//! This crate provides a NetFlow/IPFIX receiver that publishes flow records to Zenoh.
//!
//! # Supported Formats
//!
//! - NetFlow v5 (fixed format)
//! - NetFlow v7 (fixed format)
//! - NetFlow v9 (template-based)
//! - IPFIX (template-based, also known as NetFlow v10)
//!
//! # Key Expression Format
//!
//! Flow records are published to:
//! ```text
//! {prefix}/{exporter}/{src_ip}/{dst_ip}
//! ```
//!
//! For example:
//! ```text
//! zensight/netflow/router01/192_168_1_1/10_0_0_1
//! ```

pub mod config;
pub mod receiver;
