//! Zenoh sensor for system monitoring.
//!
//! This sensor collects local system metrics (CPU, memory, disk, network)
//! using the `sysinfo` crate and publishes them to Zenoh as telemetry.
//!
//! # Key Expressions
//!
//! ```text
//! zensight/v1/<origin>/telemetry/sysinfo/cpu/usage
//! zensight/v1/<origin>/telemetry/sysinfo/cpu/<core_id>/usage
//! zensight/v1/<origin>/telemetry/sysinfo/cpu/times/user
//! zensight/v1/<origin>/telemetry/sysinfo/cpu/times/system
//! zensight/v1/<origin>/telemetry/sysinfo/cpu/times/iowait
//! zensight/v1/<origin>/telemetry/sysinfo/memory/used
//! zensight/v1/<origin>/telemetry/sysinfo/memory/available
//! zensight/v1/<origin>/telemetry/sysinfo/disk/<mount>/usage
//! zensight/v1/<origin>/telemetry/sysinfo/disk/<device>/io/read_bytes
//! zensight/v1/<origin>/telemetry/sysinfo/network/<interface>/rx_bytes
//! zensight/v1/<origin>/telemetry/sysinfo/sensors/<chip>/<label>/temp
//! zensight/v1/<origin>/telemetry/sysinfo/tcp/established
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
