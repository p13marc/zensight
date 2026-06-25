//! Linux-specific metrics collection using procfs.
//!
//! This module provides detailed system metrics only available on Linux:
//! - CPU time breakdown (user/system/iowait/steal/nice/idle)
//! - Disk I/O statistics (read/write bytes, IOPS)
//! - Temperature sensors (via hwmon)
//! - TCP connection state counts

use crate::map::{
    BatteryReading, CgroupSample, ConntrackSample, DiskSaturation, EdacSample, FanReading, FdStat,
    InodeStat, KernelDerivatives, MdArray, NetDevStat, NetstatSample, PressureSample, PsiSample,
    RaplDomain, SchedstatSample, SockstatSample, SoftnetSample, VmStat, disk_saturation,
    parse_cgroup_scalar, parse_conntrack, parse_file_nr, parse_flat_kv, parse_mdstat,
    parse_net_dev, parse_netstat, parse_pressure_file, parse_schedstat, parse_sockstat,
    parse_softnet, parse_vmstat,
};
use procfs::{Current, CurrentSI};
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
    /// `io_ticks` (diskstats field 10): ms the device had I/O in flight.
    pub io_time_ms: u64,
    /// `weighted_io_ticks` (diskstats field 11): time-weighted queue length, ms.
    pub weighted_io_time_ms: u64,
}

/// Previous disk I/O stats for calculating rates and saturation deltas.
#[derive(Debug, Clone, Default)]
pub struct PrevDiskIo {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
    pub io_time_ms: u64,
    pub weighted_io_time_ms: u64,
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
    ) -> HashMap<String, (DiskIoStats, Option<DiskIoStats>, Option<DiskSaturation>)> {
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
            // diskstats field 10 (io_ticks) and field 11 (weighted io_ticks).
            let io_time_ms = disk.time_in_progress;
            let weighted_io_time_ms = disk.weighted_time_in_progress;

            let stats = DiskIoStats {
                read_bytes,
                write_bytes,
                read_ios: disk.reads,
                write_ios: disk.writes,
                io_time_ms,
                weighted_io_time_ms,
            };

            // Snapshot the previous sample (copied out) so we can derive both
            // rates and saturation gauges before overwriting it below.
            let prev = self.prev_disk_io.get(name).cloned();

            // Calculate rates if we have previous data
            let rates = match (&prev, interval_secs > 0.0) {
                (Some(prev), true) => Some(DiskIoStats {
                    read_bytes: ((read_bytes.saturating_sub(prev.read_bytes)) as f64
                        / interval_secs) as u64,
                    write_bytes: ((write_bytes.saturating_sub(prev.write_bytes)) as f64
                        / interval_secs) as u64,
                    read_ios: ((disk.reads.saturating_sub(prev.read_ios)) as f64 / interval_secs)
                        as u64,
                    write_ios: ((disk.writes.saturating_sub(prev.write_ios)) as f64 / interval_secs)
                        as u64,
                    io_time_ms: 0, // Rate doesn't make sense for cumulative time
                    weighted_io_time_ms: 0,
                }),
                _ => None,
            };

            // Derived saturation gauges (%util, queue depth) from the raw
            // io_time / weighted_io_time deltas across the poll interval.
            let saturation = prev.as_ref().map(|prev| {
                let io_delta = io_time_ms.saturating_sub(prev.io_time_ms);
                let weighted_delta = weighted_io_time_ms.saturating_sub(prev.weighted_io_time_ms);
                disk_saturation(io_delta, weighted_delta, interval_secs * 1000.0)
            });

            // Store current values for next iteration
            self.prev_disk_io.insert(
                name.clone(),
                PrevDiskIo {
                    read_bytes,
                    write_bytes,
                    read_ios: disk.reads,
                    write_ios: disk.writes,
                    io_time_ms,
                    weighted_io_time_ms,
                },
            );

            result.insert(name.clone(), (stats, rates, saturation));
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

// ===========================================================================
// Wave 1 saturation/error collectors (stateless reads → pure map sample types)
// ===========================================================================

/// Convert a procfs `PressureRecord` into our plain owned sample.
fn pressure_sample(r: &procfs::PressureRecord) -> PressureSample {
    PressureSample {
        avg10: r.avg10 as f64,
        avg60: r.avg60 as f64,
        avg300: r.avg300 as f64,
        total_us: r.total,
    }
}

/// Collect `/proc/pressure/{cpu,memory,io}` via the typed procfs PSI structs.
///
/// Returns `None` only if no resource could be read at all (e.g. kernel built
/// without `CONFIG_PSI`); individual missing resources are left as `None` so
/// the mapper skips them rather than emitting misleading zeros.
pub fn collect_psi() -> Option<PsiSample> {
    let cpu = procfs::CpuPressure::current().ok();
    let mem = procfs::MemoryPressure::current().ok();
    let io = procfs::IoPressure::current().ok();

    if cpu.is_none() && mem.is_none() && io.is_none() {
        return None;
    }

    Some(PsiSample {
        cpu_some: cpu.map(|c| pressure_sample(&c.some)),
        memory_some: mem.as_ref().map(|m| pressure_sample(&m.some)),
        memory_full: mem.as_ref().map(|m| pressure_sample(&m.full)),
        io_some: io.as_ref().map(|i| pressure_sample(&i.some)),
        io_full: io.as_ref().map(|i| pressure_sample(&i.full)),
    })
}

/// Read and parse `/proc/vmstat` into the allowlisted saturation subset.
/// Returns `None` if the file is unreadable (skip the collector this tick).
pub fn collect_vmstat() -> Option<VmStat> {
    match std::fs::read_to_string("/proc/vmstat") {
        Ok(content) => Some(parse_vmstat(&content)),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/vmstat");
            None
        }
    }
}

/// Collect `/proc/stat` derivatives from the typed `procfs::KernelStats`.
pub fn collect_kernel_derivatives() -> Option<KernelDerivatives> {
    match procfs::KernelStats::current() {
        Ok(stat) => Some(KernelDerivatives {
            context_switches: stat.ctxt,
            forks: stat.processes,
            procs_running: stat.procs_running,
            procs_blocked: stat.procs_blocked,
        }),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/stat for kernel derivatives");
            None
        }
    }
}

/// Read and parse `/proc/sys/fs/file-nr` into FD-table occupancy.
pub fn collect_fd() -> Option<FdStat> {
    match std::fs::read_to_string("/proc/sys/fs/file-nr") {
        Ok(content) => parse_file_nr(&content),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/sys/fs/file-nr");
            None
        }
    }
}

/// Collect per-mount inode occupancy via `statvfs()`. `mounts` is the already
/// filtered `(mount_point, fs_type)` list (small, bounded — fine inline). A
/// mount that fails `statvfs` (e.g. autofs not yet triggered) is skipped, and
/// filesystems reporting zero total inodes (many pseudo/btrfs mounts) are
/// omitted to avoid meaningless 0/0 series.
pub fn collect_inodes(mounts: &[(String, String)]) -> Vec<InodeStat> {
    let mut out = Vec::with_capacity(mounts.len());
    for (mount, fs_type) in mounts {
        match rustix::fs::statvfs(mount.as_str()) {
            Ok(vfs) => {
                if vfs.f_files == 0 {
                    continue;
                }
                let total = vfs.f_files;
                let free = vfs.f_ffree;
                out.push(InodeStat {
                    mount: mount.clone(),
                    fs_type: fs_type.clone(),
                    total,
                    free,
                    used: total.saturating_sub(free),
                });
            }
            Err(e) => {
                warn!(mount = %mount, error = %e, "statvfs failed; skipping inode stats");
            }
        }
    }
    out
}

/// Read and parse `/proc/net/dev` into per-interface drop/fifo/frame counters.
pub fn collect_net_dev() -> Vec<NetDevStat> {
    match std::fs::read_to_string("/proc/net/dev") {
        Ok(content) => parse_net_dev(&content),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/net/dev");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// H. USE-completeness collectors (#98)
// ---------------------------------------------------------------------------

/// Read `/proc/net/snmp` + `/proc/net/netstat` into the TCP retransmit /
/// listen-overflow saturation subset. `None` only if neither file is readable;
/// a single missing file still yields the fields the other provides.
pub fn collect_netstat() -> Option<NetstatSample> {
    let snmp = std::fs::read_to_string("/proc/net/snmp").ok();
    let netstat = std::fs::read_to_string("/proc/net/netstat").ok();
    if snmp.is_none() && netstat.is_none() {
        return None;
    }
    Some(parse_netstat(
        snmp.as_deref().unwrap_or(""),
        netstat.as_deref().unwrap_or(""),
    ))
}

/// Read `/proc/net/sockstat` into socket-occupancy gauges. `None` if unreadable.
pub fn collect_sockstat() -> Option<SockstatSample> {
    match std::fs::read_to_string("/proc/net/sockstat") {
        Ok(content) => Some(parse_sockstat(&content)),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/net/sockstat");
            None
        }
    }
}

/// Read `/proc/net/softnet_stat` into summed processed/dropped/squeezed totals.
pub fn collect_softnet() -> Option<SoftnetSample> {
    match std::fs::read_to_string("/proc/net/softnet_stat") {
        Ok(content) => parse_softnet(&content),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/net/softnet_stat");
            None
        }
    }
}

/// Read `/proc/schedstat` into per-CPU + total scheduler run-delay.
pub fn collect_schedstat() -> Option<SchedstatSample> {
    match std::fs::read_to_string("/proc/schedstat") {
        Ok(content) => parse_schedstat(&content),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/schedstat");
            None
        }
    }
}

/// Read conntrack count (+ optional max) from `/proc/sys/net/netfilter/`.
/// `None` if the count file is absent (conntrack module not loaded).
pub fn collect_conntrack() -> Option<ConntrackSample> {
    let count = std::fs::read_to_string("/proc/sys/net/netfilter/nf_conntrack_count").ok()?;
    let max = std::fs::read_to_string("/proc/sys/net/netfilter/nf_conntrack_max").ok();
    parse_conntrack(&count, max.as_deref())
}

/// Walk `/sys/devices/system/edac/mc/mc*/{ce_count,ue_count}` for per-controller
/// ECC error counts. Empty on hosts without ECC/EDAC (no `mc<N>` dirs).
pub fn collect_edac() -> Vec<EdacSample> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/edac/mc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Only memory-controller dirs: `mc` followed by digits (skip power/uevent).
        let Some(num) = name.strip_prefix("mc") else {
            continue;
        };
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let dir = entry.path();
        let ce = std::fs::read_to_string(dir.join("ce_count"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let ue = std::fs::read_to_string(dir.join("ue_count"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        // Skip a controller exposing neither counter (avoid fabricated zeros).
        if ce.is_none() && ue.is_none() {
            continue;
        }
        out.push(EdacSample {
            controller: name,
            ce: ce.unwrap_or(0),
            ue: ue.unwrap_or(0),
        });
    }
    out
}

/// Read and parse `/proc/mdstat` into per-array RAID state. Empty when the file
/// is absent or lists no md arrays.
pub fn collect_mdstat() -> Vec<MdArray> {
    match std::fs::read_to_string("/proc/mdstat") {
        Ok(content) => parse_mdstat(&content),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// E. cgroup-v2 (container saturation)
// ---------------------------------------------------------------------------

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Resolve this process's own cgroup-v2 path from `/proc/self/cgroup`. The v2
/// line is `0::<path>`; returns `<path>` (e.g. `/system.slice/foo.service`),
/// or `None` if the host is not running unified cgroup-v2.
fn own_cgroup_path() -> Option<String> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in content.lines() {
        // Format: hierarchy-ID:controller-list:cgroup-path. v2 is `0::<path>`.
        let mut parts = line.splitn(3, ':');
        let hid = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if hid == "0" && controllers.is_empty() {
            return Some(path.to_string());
        }
    }
    None
}

/// Read a single cgroup-v2 controller file relative to the cgroup dir, returning
/// its trimmed contents (or `None` if absent / unreadable — a controller may not
/// be enabled for this cgroup).
fn read_cgroup_file(cgroup_path: &str, file: &str) -> Option<String> {
    // cgroup_path is absolute-from-root (starts with `/`); join under the mount.
    let full = format!("{CGROUP_ROOT}{cgroup_path}/{file}");
    std::fs::read_to_string(full).ok()
}

/// Collect the cgroup-v2 saturation sample for a single cgroup path. Reads
/// `cpu.stat`, `memory.{current,max,events}`, and the `{cpu,memory,io}.pressure`
/// PSI files. Every missing file degrades to `None` for that field.
pub fn collect_cgroup(cgroup_path: &str) -> CgroupSample {
    let mut s = CgroupSample {
        path: cgroup_path.to_string(),
        ..Default::default()
    };

    if let Some(cpu_stat) = read_cgroup_file(cgroup_path, "cpu.stat") {
        let kv = parse_flat_kv(&cpu_stat);
        s.cpu_nr_throttled = kv.get("nr_throttled").copied();
        s.cpu_throttled_usec = kv.get("throttled_usec").copied();
    }
    if let Some(cur) = read_cgroup_file(cgroup_path, "memory.current") {
        s.memory_current = parse_cgroup_scalar(&cur);
    }
    if let Some(max) = read_cgroup_file(cgroup_path, "memory.max") {
        s.memory_max = parse_cgroup_scalar(&max);
    }
    if let Some(events) = read_cgroup_file(cgroup_path, "memory.events") {
        let kv = parse_flat_kv(&events);
        s.memory_oom_kills = kv.get("oom_kill").copied();
        s.memory_oom = kv.get("oom").copied();
    }
    if let Some(p) = read_cgroup_file(cgroup_path, "cpu.pressure") {
        let (some, _full) = parse_pressure_file(&p);
        s.cpu_pressure_some = some;
    }
    if let Some(p) = read_cgroup_file(cgroup_path, "memory.pressure") {
        let (some, full) = parse_pressure_file(&p);
        s.memory_pressure_some = some;
        s.memory_pressure_full = full;
    }
    if let Some(p) = read_cgroup_file(cgroup_path, "io.pressure") {
        let (some, full) = parse_pressure_file(&p);
        s.io_pressure_some = some;
        s.io_pressure_full = full;
    }
    s
}

/// Collect cgroup samples for the configured set of cgroups. If `extra` is empty
/// only the sensor's own cgroup is read; otherwise each configured path is read
/// too. Returns `None` only when cgroup-v2 is not present at all (no own cgroup
/// and no `cpu.stat` under the root), so the collector skips cleanly on
/// non-container / cgroup-v1 hosts.
pub fn collect_cgroups(extra: &[String]) -> Option<Vec<CgroupSample>> {
    // Cheap presence check: the unified hierarchy exposes cgroup.controllers.
    if !std::path::Path::new(&format!("{CGROUP_ROOT}/cgroup.controllers")).exists() {
        return None;
    }
    let mut out = Vec::new();
    if let Some(own) = own_cgroup_path() {
        out.push(collect_cgroup(&own));
    }
    for p in extra {
        // Avoid duplicating the own cgroup if listed explicitly.
        if out.iter().any(|s| &s.path == p) {
            continue;
        }
        out.push(collect_cgroup(p));
    }
    if out.is_empty() { None } else { Some(out) }
}

// ---------------------------------------------------------------------------
// G. Thermal / power depth (RAPL, fans, battery, entropy)
// ---------------------------------------------------------------------------

/// Read every RAPL energy zone under `/sys/class/powercap/`. Each `intel-rapl*`
/// directory has `name`, `energy_uj`, and `max_energy_range_uj`. Skips zones
/// without an energy counter. Empty on no-RAPL hardware / no permission.
pub fn collect_rapl() -> Vec<RaplDomain> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/powercap") else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let zone = entry.file_name().to_string_lossy().to_string();
        // Only energy zones expose energy_uj.
        let Some(energy_uj) = std::fs::read_to_string(dir.join("energy_uj"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        else {
            continue;
        };
        let name = std::fs::read_to_string(dir.join("name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| zone.clone());
        let max_energy_uj = std::fs::read_to_string(dir.join("max_energy_range_uj"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        out.push(RaplDomain {
            zone,
            name,
            energy_uj,
            max_energy_uj,
        });
    }
    out
}

/// Read all hwmon `fan*_input` (RPM) readings. Mirrors the temperature walk.
pub fn collect_fans() -> Vec<FanReading> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/hwmon") else {
        return out;
    };
    for entry in entries.flatten() {
        let hwmon = entry.path();
        let chip = std::fs::read_to_string(hwmon.join("name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let Ok(files) = std::fs::read_dir(&hwmon) else {
            continue;
        };
        for file in files.flatten() {
            let fname = file.file_name().to_string_lossy().to_string();
            if !fname.starts_with("fan") || !fname.ends_with("_input") {
                continue;
            }
            let num = fname
                .strip_prefix("fan")
                .and_then(|s| s.strip_suffix("_input"))
                .unwrap_or("0");
            let Some(rpm) = std::fs::read_to_string(hwmon.join(&fname))
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
            else {
                continue;
            };
            // A zero reading is usually an unconnected header; skip the noise.
            if rpm == 0.0 {
                continue;
            }
            let label = std::fs::read_to_string(hwmon.join(format!("fan{num}_label")))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| format!("fan{num}"));
            out.push(FanReading {
                chip: chip.clone(),
                label,
                rpm,
            });
        }
    }
    out
}

/// Read battery / power-supply state from `/sys/class/power_supply/*` for
/// supplies of type `Battery` (capacity + status). AC adapters are skipped.
pub fn collect_batteries() -> Vec<BatteryReading> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let kind = std::fs::read_to_string(dir.join("type"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if kind != "Battery" {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let capacity = std::fs::read_to_string(dir.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok());
        let status = std::fs::read_to_string(dir.join("status"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        out.push(BatteryReading {
            name,
            capacity,
            status,
        });
    }
    out
}

/// Read the kernel entropy pool fill (`/proc/sys/kernel/random/entropy_avail`).
pub fn collect_entropy() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/kernel/random/entropy_avail")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

// ---------------------------------------------------------------------------
// Memory composition (/proc/meminfo) for MemAvailable-based pressure.
// ---------------------------------------------------------------------------

/// Memory composition (bytes) from `/proc/meminfo`. These break down where the
/// non-`MemAvailable` memory has gone (reclaimable cache vs. genuinely used),
/// so a high `usage_percent` can be attributed to real pressure or just cache.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemComposition {
    pub cached: u64,
    pub buffers: u64,
    pub slab: u64,
    pub dirty: u64,
    pub writeback: u64,
}

/// Read `/proc/meminfo` via typed `procfs::Meminfo` (values already in bytes).
/// Returns `None` if the file is unreadable (skip the gauges this tick).
pub fn collect_mem_composition() -> Option<MemComposition> {
    match procfs::Meminfo::current() {
        Ok(mi) => Some(MemComposition {
            cached: mi.cached,
            buffers: mi.buffers,
            slab: mi.slab,
            dirty: mi.dirty,
            writeback: mi.writeback,
        }),
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/meminfo for memory composition");
            None
        }
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
        for (name, (stats, _rates, _sat)) in disk_io {
            assert!(!name.is_empty());
            let _ = stats.read_bytes;
            let _ = stats.write_bytes;
        }
    }
}
