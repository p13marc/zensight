//! SNMP sensor library: config schema, MIB resolution, device poller, trap receiver.
//!
//! The binary entry point lives in `main.rs`; the library exists so integration
//! tests can drive the poller against an in-process SNMP agent.

pub mod alerts;
pub mod config;
pub mod interfaces;
pub mod mib;
pub mod oid;
pub mod poller;
pub mod profile;
pub mod rate;
pub mod smi;
pub mod trap;
