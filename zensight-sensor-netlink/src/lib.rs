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
pub mod events;
pub mod map;
pub mod query;
pub mod route_history;
pub mod sentinel;
pub mod xfrm_sentinel;

/// Opt-in eBPF module (#114). Compiled only with `--features ebpf`; the rest of
/// the crate stays aya-free.
#[cfg(feature = "ebpf")]
pub mod ebpf;

pub use collector::Collector;
pub use collector::MetricCache;
pub use config::{NetlinkConfig, NetlinkSensorConfig};
pub use sentinel::{Evaluator, ExpectationsConfig, MetricExpectation, SentinelHandle};
pub use xfrm_sentinel::{XfrmSentinel, XfrmSentinelConfig};
