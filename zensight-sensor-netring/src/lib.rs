//! ZenSight netring sensor.
//!
//! Streams wire-level telemetry (flows, per-app bandwidth) and network-anomaly
//! alerts (port scan, ...) from zero-copy capture via
//! [`netring`](https://github.com/p13marc/netring), published under
//! `zensight/netring/<sensor>/...` and `zensight/netring/@/alerts/*`.
//!
//! Live capture needs `CAP_NET_RAW` (+`CAP_IPC_LOCK` for AF_XDP); offline pcap
//! replay (`netring.pcap`) needs no privileges. Linux only.

pub mod config;
pub mod map;
pub mod monitor;
pub mod publish;

pub use config::{NetringConfig, NetringSensorConfig};
