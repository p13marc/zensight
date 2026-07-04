//! Zenoh sensor for system monitoring.
//!
//! This sensor collects local system metrics (CPU, memory, disk, network)
//! using the `sysinfo` crate and publishes them to Zenoh as telemetry.
//!
//! # Key Expressions
//!
//! ```text
//! zensight/sysinfo/<source>/cpu/usage
//! zensight/sysinfo/<source>/cpu/<core_id>/usage
//! zensight/sysinfo/<source>/cpu/times/user
//! zensight/sysinfo/<source>/cpu/times/system
//! zensight/sysinfo/<source>/cpu/times/iowait
//! zensight/sysinfo/<source>/memory/used
//! zensight/sysinfo/<source>/memory/available
//! zensight/sysinfo/<source>/disk/<mount>/usage
//! zensight/sysinfo/<source>/disk/<device>/io/read_bytes
//! zensight/sysinfo/<source>/network/<interface>/rx_bytes
//! zensight/sysinfo/<source>/sensors/<chip>/<label>/temp
//! zensight/sysinfo/<source>/tcp/established
//! ```

pub mod alerts;
pub mod collector;
pub mod config;
pub mod map;
pub mod query;
pub mod saturation;

#[cfg(target_os = "linux")]
pub mod linux;

/// Opt-in eBPF saturation histograms (#99). Compiled only with `--features
/// ebpf` on Linux; the rest of the crate stays aya-free.
#[cfg(all(target_os = "linux", feature = "ebpf"))]
pub mod ebpf;
