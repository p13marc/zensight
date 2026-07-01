//! ZenSight systemd sensor library.
//!
//! Reads systemd unit/service state aggregates and boot-performance timings from
//! the `org.freedesktop.systemd1.Manager` D-Bus interface (system bus) and
//! publishes them as [`zensight_common::TelemetryPoint`]s under
//! `zensight/systemd/<host>/…`.
//!
//! The pure mapping (D-Bus property structs → telemetry) lives in [`collector`]
//! as free functions so it is unit-testable without a live bus.

pub mod action;
pub mod alerts;
pub mod cgroup;
pub mod collector;
pub mod command;
pub mod config;
pub mod dbus;
pub mod events;
pub mod journal;
pub mod map;
pub mod query;
pub mod sentinel;
pub mod unit;
