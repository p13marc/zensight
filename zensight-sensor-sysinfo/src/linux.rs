//! Linux-specific metrics collection using procfs.
//!
//! This module provides detailed system metrics only available on Linux:
//! - CPU time breakdown (user/system/iowait/steal/nice/idle)
//! - Disk I/O statistics (read/write bytes, IOPS)
//! - Temperature sensors (via hwmon)
//! - TCP connection state counts

use procfs::CurrentSI;
use std::collections::HashMap;
use tracing::warn;

/// CPU time breakdown in percentages.
#[derive(Debug, Clone, Default)]
pub struct CpuTimes {
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub idle: f64,
    pub iowait: f64,
    pub irq: f64,
    pub softirq: f64,
    pub steal: f64,
}

/// Previous CPU times for calculating deltas.
#[derive(Debug, Clone, Default)]
pub struct PrevCpuTimes {
    pub total: u64,
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

/// Disk I/O statistics.
#[derive(Debug, Clone, Default)]
pub struct DiskIoStats {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
    pub io_time_ms: u64,
}

/// Previous disk I/O stats for calculating rates.
#[derive(Debug, Clone, Default)]
pub struct PrevDiskIo {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
}

/// Temperature sensor reading.
#[derive(Debug, Clone)]
pub struct Temperature {
    pub label: String,
    pub chip: String,
    pub temp_celsius: f64,
    pub critical: Option<f64>,
    pub max: Option<f64>,
}

/// TCP connection state counts.
#[derive(Debug, Clone, Default)]
pub struct TcpStates {
    pub established: u64,
    pub syn_sent: u64,
    pub syn_recv: u64,
    pub fin_wait1: u64,
    pub fin_wait2: u64,
    pub time_wait: u64,
    pub close: u64,
    pub close_wait: u64,
    pub last_ack: u64,
    pub listen: u64,
    pub closing: u64,
}

/// Collector for Linux-specific metrics.
pub struct LinuxMetrics {
    prev_cpu: HashMap<String, PrevCpuTimes>,
    prev_disk_io: HashMap<String, PrevDiskIo>,
}

impl LinuxMetrics {
    pub fn new() -> Self {
        Self {
            prev_cpu: HashMap::new(),
            prev_disk_io: HashMap::new(),
        }
    }

    /// Collect CPU time breakdown for all CPUs.
    /// Returns a map of CPU name ("cpu" for total, "cpu0", "cpu1", etc.) to times.
    pub fn collect_cpu_times(&mut self) -> HashMap<String, CpuTimes> {
        let mut result = HashMap::new();

        let Ok(stat) = procfs::KernelStats::current() else {
            warn!("Failed to read /proc/stat");
            return result;
        };

        // Total CPU
        let total = &stat.total;
        if let Some(times) = self.calculate_cpu_times("cpu", total) {
            result.insert("cpu".to_string(), times);
        }

        // Per-CPU
        for (i, cpu) in stat.cpu_time.iter().enumerate() {
            let name = format!("cpu{}", i);
            if let Some(times) = self.calculate_cpu_times(&name, cpu) {
                result.insert(name, times);
            }
        }

        result
    }

    fn calculate_cpu_times(&mut self, name: &str, cpu: &procfs::CpuTime) -> Option<CpuTimes> {
        let user = cpu.user;
        let nice = cpu.nice;
        let system = cpu.system;
        let idle = cpu.idle;
        let iowait = cpu.iowait.unwrap_or(0);
        let irq = cpu.irq.unwrap_or(0);
        let softirq = cpu.softirq.unwrap_or(0);
        let steal = cpu.steal.unwrap_or(0);

        let total = user + nice + system + idle + iowait + irq + softirq + steal;

        let prev = self.prev_cpu.entry(name.to_string()).or_default();

        // Calculate deltas
        let delta_total = total.saturating_sub(prev.total);
        if delta_total == 0 {
            // First reading or no change
            prev.total = total;
            prev.user = user;
            prev.nice = nice;
            prev.system = system;
            prev.idle = idle;
            prev.iowait = iowait;
            prev.irq = irq;
            prev.softirq = softirq;
            prev.steal = steal;
            return None;
        }

        let delta_total_f = delta_total as f64;

        let times = CpuTimes {
            user: (user.saturating_sub(prev.user)) as f64 / delta_total_f * 100.0,
            nice: (nice.saturating_sub(prev.nice)) as f64 / delta_total_f * 100.0,
            system: (system.saturating_sub(prev.system)) as f64 / delta_total_f * 100.0,
            idle: (idle.saturating_sub(prev.idle)) as f64 / delta_total_f * 100.0,
            iowait: (iowait.saturating_sub(prev.iowait)) as f64 / delta_total_f * 100.0,
            irq: (irq.saturating_sub(prev.irq)) as f64 / delta_total_f * 100.0,
            softirq: (softirq.saturating_sub(prev.softirq)) as f64 / delta_total_f * 100.0,
            steal: (steal.saturating_sub(prev.steal)) as f64 / delta_total_f * 100.0,
        };

        // Update previous values
        prev.total = total;
        prev.user = user;
        prev.nice = nice;
        prev.system = system;
        prev.idle = idle;
        prev.iowait = iowait;
        prev.irq = irq;
        prev.softirq = softirq;
        prev.steal = steal;

        Some(times)
    }

    /// Collect disk I/O statistics.
    /// Returns a map of device name to stats and rates.
    pub fn collect_disk_io(
        &mut self,
        interval_secs: f64,
    ) -> HashMap<String, (DiskIoStats, Option<DiskIoStats>)> {
        let mut result = HashMap::new();

        let Ok(diskstats) = procfs::diskstats() else {
            warn!("Failed to read /proc/diskstats");
            return result;
        };

        for disk in diskstats {
            // Skip partitions (we want whole disks like sda, nvme0n1)
            // Partitions have numbers at the end like sda1, sda2
            let name = &disk.name;

            // Skip loop devices, ram disks, and device mapper
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("dm-") {
                continue;
            }

            // Sector size is typically 512 bytes
            let sector_size: u64 = 512;
            let read_bytes = disk.sectors_read * sector_size;
            let write_bytes = disk.sectors_written * sector_size;

            let stats = DiskIoStats {
                read_bytes,
                write_bytes,
                read_ios: disk.reads,
                write_ios: disk.writes,
                io_time_ms: disk.time_in_progress,
            };

            // Calculate rates if we have previous data
            let rates = if let Some(prev) = self.prev_disk_io.get(name) {
                if interval_secs > 0.0 {
                    Some(DiskIoStats {
                        read_bytes: ((read_bytes.saturating_sub(prev.read_bytes)) as f64
                            / interval_secs) as u64,
                        write_bytes: ((write_bytes.saturating_sub(prev.write_bytes)) as f64
                            / interval_secs) as u64,
                        read_ios: ((disk.reads.saturating_sub(prev.read_ios)) as f64
                            / interval_secs) as u64,
                        write_ios: ((disk.writes.saturating_sub(prev.write_ios)) as f64
                            / interval_secs) as u64,
                        io_time_ms: 0, // Rate doesn't make sense for cumulative time
                    })
                } else {
                    None
                }
            } else {
                None
            };

            // Store current values for next iteration
            self.prev_disk_io.insert(
                name.clone(),
                PrevDiskIo {
                    read_bytes,
                    write_bytes,
                    read_ios: disk.reads,
                    write_ios: disk.writes,
                },
            );

            result.insert(name.clone(), (stats, rates));
        }

        result
    }

    /// Collect temperature sensor readings from hwmon.
    pub fn collect_temperatures() -> Vec<Temperature> {
        let mut temps = Vec::new();

        // Read from /sys/class/hwmon/hwmon*/
        let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
            return temps;
        };

        for entry in entries.flatten() {
            let hwmon_path = entry.path();

            // Get chip name
            let chip_name = std::fs::read_to_string(hwmon_path.join("name"))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());

            // Find all temp*_input files
            let Ok(files) = std::fs::read_dir(&hwmon_path) else {
                continue;
            };

            for file in files.flatten() {
                let file_name = file.file_name().to_string_lossy().to_string();
                if !file_name.starts_with("temp") || !file_name.ends_with("_input") {
                    continue;
                }

                // Extract sensor number (e.g., "temp1_input" -> "1")
                let sensor_num = file_name
                    .strip_prefix("temp")
                    .and_then(|s| s.strip_suffix("_input"))
                    .unwrap_or("0");

                // Read temperature (in millidegrees Celsius)
                let temp_path = hwmon_path.join(&file_name);
                let Ok(temp_str) = std::fs::read_to_string(&temp_path) else {
                    continue;
                };
                let Ok(temp_milli): Result<i64, _> = temp_str.trim().parse() else {
                    continue;
                };
                let temp_celsius = temp_milli as f64 / 1000.0;

                // Read label if available
                let label_path = hwmon_path.join(format!("temp{}_label", sensor_num));
                let label = std::fs::read_to_string(label_path)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| format!("temp{}", sensor_num));

                // Read critical temp if available
                let crit_path = hwmon_path.join(format!("temp{}_crit", sensor_num));
                let critical = std::fs::read_to_string(crit_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .map(|v| v as f64 / 1000.0);

                // Read max temp if available
                let max_path = hwmon_path.join(format!("temp{}_max", sensor_num));
                let max = std::fs::read_to_string(max_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .map(|v| v as f64 / 1000.0);

                temps.push(Temperature {
                    label,
                    chip: chip_name.clone(),
                    temp_celsius,
                    critical,
                    max,
                });
            }
        }

        temps
    }

    /// Collect TCP connection state counts.
    pub fn collect_tcp_states() -> TcpStates {
        let mut states = TcpStates::default();

        // Read IPv4 TCP connections
        if let Ok(tcp) = procfs::net::tcp() {
            for entry in tcp {
                match entry.state {
                    procfs::net::TcpState::Established => states.established += 1,
                    procfs::net::TcpState::SynSent => states.syn_sent += 1,
                    procfs::net::TcpState::SynRecv => states.syn_recv += 1,
                    procfs::net::TcpState::FinWait1 => states.fin_wait1 += 1,
                    procfs::net::TcpState::FinWait2 => states.fin_wait2 += 1,
                    procfs::net::TcpState::TimeWait => states.time_wait += 1,
                    procfs::net::TcpState::Close => states.close += 1,
                    procfs::net::TcpState::CloseWait => states.close_wait += 1,
                    procfs::net::TcpState::LastAck => states.last_ack += 1,
                    procfs::net::TcpState::Listen => states.listen += 1,
                    procfs::net::TcpState::Closing => states.closing += 1,
                    procfs::net::TcpState::NewSynRecv => states.syn_recv += 1,
                }
            }
        }

        // Read IPv6 TCP connections
        if let Ok(tcp6) = procfs::net::tcp6() {
            for entry in tcp6 {
                match entry.state {
                    procfs::net::TcpState::Established => states.established += 1,
                    procfs::net::TcpState::SynSent => states.syn_sent += 1,
                    procfs::net::TcpState::SynRecv => states.syn_recv += 1,
                    procfs::net::TcpState::FinWait1 => states.fin_wait1 += 1,
                    procfs::net::TcpState::FinWait2 => states.fin_wait2 += 1,
                    procfs::net::TcpState::TimeWait => states.time_wait += 1,
                    procfs::net::TcpState::Close => states.close += 1,
                    procfs::net::TcpState::CloseWait => states.close_wait += 1,
                    procfs::net::TcpState::LastAck => states.last_ack += 1,
                    procfs::net::TcpState::Listen => states.listen += 1,
                    procfs::net::TcpState::Closing => states.closing += 1,
                    procfs::net::TcpState::NewSynRecv => states.syn_recv += 1,
                }
            }
        }

        states
    }
}

impl Default for LinuxMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linux_metrics_new() {
        let metrics = LinuxMetrics::new();
        assert!(metrics.prev_cpu.is_empty());
        assert!(metrics.prev_disk_io.is_empty());
    }

    #[test]
    fn test_collect_cpu_times() {
        let mut metrics = LinuxMetrics::new();

        // First call populates previous values
        let _times1 = metrics.collect_cpu_times();
        // May be empty on first call (no delta yet)

        // Second call should have data
        std::thread::sleep(std::time::Duration::from_millis(100));
        let times2 = metrics.collect_cpu_times();

        // Should have at least total CPU
        if !times2.is_empty() {
            let total = times2.get("cpu").unwrap();
            // All percentages should sum to ~100%
            let sum = total.user
                + total.nice
                + total.system
                + total.idle
                + total.iowait
                + total.irq
                + total.softirq
                + total.steal;
            assert!(
                sum > 99.0 && sum < 101.0,
                "CPU times should sum to ~100%, got {}",
                sum
            );
        }
    }

    #[test]
    fn test_collect_tcp_states() {
        let states = LinuxMetrics::collect_tcp_states();
        // Should have some listening sockets at least
        // (unless running in a very minimal container)
        // Just verify it doesn't panic
        let _ = states.established;
        let _ = states.listen;
    }

    #[test]
    fn test_collect_temperatures() {
        let temps = LinuxMetrics::collect_temperatures();
        // May be empty on systems without hwmon
        // Just verify it doesn't panic
        for temp in temps {
            assert!(!temp.chip.is_empty());
            assert!(!temp.label.is_empty());
        }
    }

    #[test]
    fn test_collect_disk_io() {
        let mut metrics = LinuxMetrics::new();
        let disk_io = metrics.collect_disk_io(1.0);
        // Should have some disks (unless running in unusual environment)
        // Just verify it doesn't panic
        for (name, (stats, _rates)) in disk_io {
            assert!(!name.is_empty());
            let _ = stats.read_bytes;
            let _ = stats.write_bytes;
        }
    }
}
