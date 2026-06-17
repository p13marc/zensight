//! ZenSight netlink sensor.
//!
//! Streams Linux kernel networking ground truth as telemetry under
//! `zensight/netlink/<host>/...`:
//! - `iface/<name>/<stat>` — per-interface counters and state
//! - `sockets/tcp/<state>` — TCP socket-state aggregates
//!
//! Reads are unprivileged (no `CAP_NET_ADMIN` needed). Linux only.

pub mod collector;
pub mod command;
pub mod config;
pub mod map;
pub mod query;
pub mod sentinel;

pub use collector::Collector;
pub use config::{NetlinkConfig, NetlinkSensorConfig};
pub use collector::MetricCache;
pub use sentinel::{Evaluator, ExpectationsConfig, MetricExpectation, SentinelHandle};
