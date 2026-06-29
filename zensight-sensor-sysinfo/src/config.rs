//! Configuration for the sysinfo sensor.

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use zensight_sensor_core::{SensorConfig, SensorError, ZenohConfig};

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    Parse(#[from] json5::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

/// Complete sensor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysinfoSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Sysinfo collection settings.
    pub sysinfo: SysinfoConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// On-demand debug-report (`@/report`) limits. Disabled by default.
    #[serde(default)]
    pub report: zensight_sensor_core::ReportLimits,

    /// Tier-2 directory-snapshot (`@/snapshot`) limits. Disabled by default.
    #[serde(default)]
    pub snapshot: zensight_sensor_core::SnapshotLimits,
}

/// System information collection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SysinfoConfig {
    /// Key expression prefix (default: "zensight/sysinfo").
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,

    /// Hostname to use in key expressions.
    /// Use "auto" to detect automatically (default).
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Poll interval in seconds (default: 5).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Which metrics to collect.
    #[serde(default)]
    pub collect: CollectConfig,

    /// Network interface filters.
    #[serde(default)]
    pub network: NetworkConfig,

    /// Disk mount filters.
    #[serde(default)]
    pub disk: DiskConfig,

    /// Threshold-based alerting (OOM / PSI / disk / FD / thermal / swap).
    #[serde(default)]
    pub alerts: crate::alerts::AlertsConfig,

    /// Derived host saturation-score blend + health-state bands (P6). Gated by
    /// `collect.saturation_score`.
    #[serde(default)]
    pub saturation: crate::saturation::SaturationConfig,
}

fn default_key_prefix() -> String {
    "zensight/sysinfo".to_string()
}

fn default_hostname() -> String {
    "auto".to_string()
}

fn default_poll_interval() -> u64 {
    5
}

/// Configuration for which metrics to collect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Collect CPU metrics (usage per core, frequency).
    #[serde(default = "default_true")]
    pub cpu: bool,

    /// Collect detailed CPU time breakdown (user/system/iowait/steal/nice/idle).
    /// Only available on Linux. Requires reading /proc/stat.
    #[serde(default = "default_true")]
    pub cpu_times: bool,

    /// Collect memory metrics (used, available, swap).
    #[serde(default = "default_true")]
    pub memory: bool,

    /// Collect disk metrics (usage per mount point).
    #[serde(default = "default_true")]
    pub disk: bool,

    /// Collect disk I/O stats (read/write bytes, IOPS).
    /// Only available on Linux. Requires reading /proc/diskstats.
    #[serde(default = "default_true")]
    pub disk_io: bool,

    /// Collect network metrics (bytes/packets in/out per interface).
    #[serde(default = "default_true")]
    pub network: bool,

    /// Collect system info (uptime, load averages).
    #[serde(default = "default_true")]
    pub system: bool,

    /// Collect temperature sensor readings.
    /// Only available on Linux with hwmon support.
    #[serde(default)]
    pub temperatures: bool,

    /// Collect TCP connection state counts (ESTABLISHED, TIME_WAIT, etc.).
    /// Only available on Linux. Requires reading /proc/net/tcp.
    #[serde(default)]
    pub tcp_states: bool,

    /// Collect process metrics (top N by CPU/memory).
    /// Can be resource-intensive on systems with many processes.
    #[serde(default)]
    pub processes: bool,

    /// Number of top processes to report (default: 10).
    #[serde(default = "default_top_processes")]
    pub top_processes: usize,

    /// Collect Pressure Stall Information (`/proc/pressure/{cpu,memory,io}`).
    /// Only available on Linux 4.20+ with `CONFIG_PSI`. The #1 saturation
    /// signal (USE method). Absent file => skipped gracefully.
    #[serde(default = "default_true")]
    pub pressure: bool,

    /// Collect the vmstat saturation allowlist (`oom_kill`, `pgmajfault`,
    /// `pswpin/out`, ...) plus `/proc/stat` derivatives (context switches,
    /// forks, run-queue depth). Only available on Linux.
    #[serde(default = "default_true")]
    pub vmstat: bool,

    /// Collect file-descriptor and inode saturation ceilings
    /// (`/proc/sys/fs/file-nr` + per-mount `statvfs()`). Only available on
    /// Linux. Cheap metrics that catch silent table-exhaustion outages.
    #[serde(default = "default_true")]
    pub fd_inode: bool,

    /// Collect richer per-interface `/proc/net/dev` saturation counters
    /// (rx/tx drops, fifo, frame, collisions) the `sysinfo` counters omit.
    /// Only available on Linux.
    #[serde(default = "default_true")]
    pub net_dev_extended: bool,

    /// Collect cgroup-v2 container-saturation metrics (CPU throttling, memory
    /// limit/OOM, per-cgroup pressure) from `/sys/fs/cgroup`. Default off
    /// (opt-in for container hosts). Reads the sensor's own cgroup plus any in
    /// `cgroup_paths`. Absent cgroup-v2 => skipped gracefully.
    #[serde(default)]
    pub cgroups: bool,

    /// Extra cgroup-v2 paths to monitor in addition to the sensor's own
    /// (e.g. `["/system.slice/foo.service"]`). Only used when `cgroups` is on.
    #[serde(default)]
    pub cgroup_paths: Vec<String>,

    /// Collect thermal/power depth: RAPL energy->watts, hwmon fan RPM, battery
    /// capacity/status, kernel entropy pool. Default off (hardware-specific,
    /// higher cardinality). Missing hardware/files => skipped gracefully.
    #[serde(default)]
    pub power: bool,

    /// Serve the on-demand per-process detail query channel
    /// (`@/query/processes?sort=cpu|mem|io&top=N`). Default on. The per-pid
    /// firehose is served only on query (P2); the small `system/processes_*`
    /// aggregates still stream via the `processes` collector.
    #[serde(default = "default_true")]
    pub process_query: bool,

    /// Collect TCP retransmit / listen-overflow errors + socket occupancy from
    /// `/proc/net/{snmp,netstat,sockstat}` (USE network errors, #98). Cheap,
    /// unprivileged, Linux-only. Missing files => skipped gracefully.
    #[serde(default = "default_true")]
    pub netstat: bool,

    /// Collect softnet backlog drops / time-squeezes from
    /// `/proc/net/softnet_stat` (NIC→kernel backpressure, #98). Linux-only.
    #[serde(default = "default_true")]
    pub softnet: bool,

    /// Collect per-CPU scheduler run-delay from `/proc/schedstat` (the canonical
    /// CPU saturation signal, #98). Linux-only.
    #[serde(default = "default_true")]
    pub schedstat: bool,

    /// Collect conntrack table fill from `/proc/sys/net/netfilter/
    /// nf_conntrack_{count,max}` (#98). Linux-only; absent when the netfilter
    /// conntrack module is not loaded => skipped gracefully.
    #[serde(default = "default_true")]
    pub conntrack: bool,

    /// Collect ECC memory errors from `/sys/devices/system/edac/mc/mc*/
    /// {ce_count,ue_count}` (#98). Linux-only; no ECC hardware => emits nothing.
    #[serde(default = "default_true")]
    pub edac: bool,

    /// Collect software-RAID degraded/failed state from `/proc/mdstat` (#98).
    /// Linux-only; no md arrays => emits nothing.
    #[serde(default = "default_true")]
    pub mdadm: bool,

    /// Emit the derived host saturation score (`system/saturation_score`, 0..100)
    /// and coarse health state (`system/health_state`: ok/warn/crit) each tick
    /// (P6). Cheap — derived from already-collected USE saturation signals.
    #[serde(default = "default_true")]
    pub saturation_score: bool,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            cpu: true,
            cpu_times: true,
            memory: true,
            disk: true,
            disk_io: true,
            network: true,
            system: true,
            temperatures: false,
            tcp_states: false,
            processes: false,
            top_processes: 10,
            pressure: true,
            vmstat: true,
            fd_inode: true,
            net_dev_extended: true,
            cgroups: false,
            cgroup_paths: Vec::new(),
            power: false,
            process_query: true,
            netstat: true,
            softnet: true,
            schedstat: true,
            conntrack: true,
            edac: true,
            mdadm: true,
            saturation_score: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_top_processes() -> usize {
    10
}

/// Network interface filtering configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Only include these interfaces (empty = include all).
    #[serde(default)]
    pub include: Vec<String>,

    /// Exclude these interfaces.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Exclude loopback interfaces (default: true).
    #[serde(default = "default_true")]
    pub exclude_loopback: bool,

    /// Exclude virtual interfaces (docker, veth, etc.) (default: false).
    #[serde(default)]
    pub exclude_virtual: bool,
}

/// Disk mount filtering configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskConfig {
    /// Only include these mount points (empty = include all).
    #[serde(default)]
    pub include: Vec<String>,

    /// Exclude these mount points.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// Exclude pseudo filesystems (tmpfs, devtmpfs, etc.) (default: true).
    #[serde(default = "default_true")]
    pub exclude_pseudo: bool,
}

// Re-export LoggingConfig from the framework (they're compatible)
pub use zensight_sensor_core::LoggingConfig;

impl SysinfoSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: SysinfoSensorConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sysinfo.poll_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "poll_interval_secs must be > 0".to_string(),
            ));
        }

        // At least one metric type should be enabled
        let collect = &self.sysinfo.collect;
        if !collect.cpu
            && !collect.cpu_times
            && !collect.memory
            && !collect.disk
            && !collect.disk_io
            && !collect.network
            && !collect.system
            && !collect.temperatures
            && !collect.tcp_states
            && !collect.processes
        {
            return Err(ConfigError::Validation(
                "At least one metric type must be enabled".to_string(),
            ));
        }

        Ok(())
    }

    /// Get the hostname to use, resolving "auto" if needed.
    pub fn get_hostname(&self) -> String {
        if self.sysinfo.hostname == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.sysinfo.hostname.clone()
        }
    }
}

/// Implement SensorConfig trait for framework integration.
impl SensorConfig for SysinfoSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn key_prefix(&self) -> &str {
        &self.sysinfo.key_prefix
    }

    fn report_limits(&self) -> zensight_sensor_core::ReportLimits {
        self.report.clone()
    }

    fn snapshot_limits(&self) -> zensight_sensor_core::SnapshotLimits {
        self.snapshot.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        // Call our existing validate method and convert the error
        Self::validate(self).map_err(|e| SensorError::validation(e.to_string()))
    }
}

impl NetworkConfig {
    /// Check if an interface should be included.
    pub fn should_include(&self, name: &str) -> bool {
        // Check explicit include list
        if !self.include.is_empty() && !self.include.iter().any(|i| i == name) {
            return false;
        }

        // Check exclude list
        if self.exclude.iter().any(|e| e == name) {
            return false;
        }

        // Check loopback
        if self.exclude_loopback && name == "lo" {
            return false;
        }

        // Check virtual interfaces
        if self.exclude_virtual {
            let virtual_prefixes = ["docker", "veth", "br-", "virbr", "vnet"];
            if virtual_prefixes.iter().any(|p| name.starts_with(p)) {
                return false;
            }
        }

        true
    }
}

impl DiskConfig {
    /// Check if a mount point should be included.
    pub fn should_include(&self, mount_point: &str, fs_type: &str) -> bool {
        // Check explicit include list
        if !self.include.is_empty() && !self.include.iter().any(|i| i == mount_point) {
            return false;
        }

        // Check exclude list
        if self.exclude.iter().any(|e| e == mount_point) {
            return false;
        }

        // Check pseudo filesystems
        if self.exclude_pseudo {
            let pseudo_types = [
                "tmpfs",
                "devtmpfs",
                "devfs",
                "sysfs",
                "proc",
                "cgroup",
                "cgroup2",
                "securityfs",
                "debugfs",
                "configfs",
                "fusectl",
                "hugetlbfs",
                "mqueue",
                "pstore",
                "binfmt_misc",
                "autofs",
                "overlay",
                "squashfs",
            ];
            if pseudo_types.contains(&fs_type) {
                return false;
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {}
        }"#;

        let config: SysinfoSensorConfig = json5::from_str(json).unwrap();
        assert_eq!(config.sysinfo.key_prefix, "zensight/sysinfo");
        assert_eq!(config.sysinfo.hostname, "auto");
        assert_eq!(config.sysinfo.poll_interval_secs, 5);
        assert!(config.sysinfo.collect.cpu);
        assert!(config.sysinfo.collect.memory);
        assert!(!config.sysinfo.collect.processes);
        // Alerting defaults: on, with thermal opted out (needs temperatures).
        assert!(config.sysinfo.alerts.enabled);
        assert!(config.sysinfo.alerts.oom.enabled);
        assert!(!config.sysinfo.alerts.thermal.enabled);
        assert_eq!(config.sysinfo.alerts.disk.warn_percent, 90.0);
        assert_eq!(config.sysinfo.alerts.fd.warn_percent, 80.0);
    }

    #[test]
    fn test_parse_alerts_block() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {
                alerts: {
                    enabled: true,
                    for_secs: 30,
                    disk: { warn_percent: 80, critical_percent: 92 },
                    thermal: { enabled: true, fraction: 0.85 },
                    swap: { warn_pages_per_sec: 2000 },
                }
            }
        }"#;
        let config: SysinfoSensorConfig = json5::from_str(json).unwrap();
        let a = &config.sysinfo.alerts;
        assert_eq!(a.for_secs, 30);
        assert_eq!(a.disk.warn_percent, 80.0);
        assert_eq!(a.disk.critical_percent, 92.0);
        assert!(a.thermal.enabled);
        assert_eq!(a.thermal.fraction, 0.85);
        assert_eq!(a.swap.warn_pages_per_sec, 2000.0);
        // Unspecified rules keep their defaults.
        assert!(a.pressure.enabled);
        assert_eq!(a.pressure.cpu_warn, 40.0);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {
                key_prefix: "metrics/host",
                hostname: "server01",
                poll_interval_secs: 10,
                collect: {
                    cpu: true,
                    memory: true,
                    disk: true,
                    network: true,
                    system: true,
                    processes: true,
                    top_processes: 5
                },
                network: {
                    exclude: ["docker0"],
                    exclude_loopback: true,
                    exclude_virtual: true
                },
                disk: {
                    exclude: ["/boot"],
                    exclude_pseudo: true
                }
            }
        }"#;

        let config: SysinfoSensorConfig = json5::from_str(json).unwrap();
        config.validate().unwrap();

        assert_eq!(config.sysinfo.hostname, "server01");
        assert_eq!(config.sysinfo.poll_interval_secs, 10);
        assert!(config.sysinfo.collect.processes);
        assert_eq!(config.sysinfo.collect.top_processes, 5);
        assert!(config.sysinfo.network.exclude_virtual);
    }

    #[test]
    fn test_validate_zero_interval() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: { poll_interval_secs: 0 }
        }"#;

        let config: SysinfoSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_no_metrics() {
        let json = r#"{
            zenoh: { mode: "peer" },
            sysinfo: {
                collect: {
                    cpu: false,
                    cpu_times: false,
                    memory: false,
                    disk: false,
                    disk_io: false,
                    network: false,
                    system: false,
                    temperatures: false,
                    tcp_states: false,
                    processes: false
                }
            }
        }"#;

        let config: SysinfoSensorConfig = json5::from_str(json).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_network_filter() {
        let config = NetworkConfig {
            include: vec![],
            exclude: vec!["docker0".to_string()],
            exclude_loopback: true,
            exclude_virtual: true,
        };

        assert!(config.should_include("eth0"));
        assert!(!config.should_include("lo"));
        assert!(!config.should_include("docker0"));
        assert!(!config.should_include("veth123"));
        assert!(!config.should_include("br-abc"));
    }

    #[test]
    fn test_disk_filter() {
        let config = DiskConfig {
            include: vec![],
            exclude: vec!["/boot".to_string()],
            exclude_pseudo: true,
        };

        assert!(config.should_include("/", "ext4"));
        assert!(config.should_include("/home", "ext4"));
        assert!(!config.should_include("/boot", "ext4"));
        assert!(!config.should_include("/run", "tmpfs"));
        assert!(!config.should_include("/sys", "sysfs"));
    }
}
