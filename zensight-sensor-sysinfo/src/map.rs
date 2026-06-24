//! Pure, platform-agnostic sample structs, parsers and mappers.
//!
//! Every saturation/error collector follows the Plan-05 pipeline:
//! `typed sample struct → pure map fn → TelemetryPoint`. The structs and
//! functions in this module are deliberately free of any `/proc` or `/sys`
//! access so they can be unit-tested on synthetic fixtures with no privileges
//! and on any platform. The Linux-only `linux.rs` module decodes the kernel /
//! `procfs` types into these plain owned structs and calls these mappers; the
//! collector turns the resulting [`Metric`]s into wire `TelemetryPoint`s.

use zensight_common::telemetry::TelemetryValue;

/// One mapped metric, prior to becoming a wire `TelemetryPoint`. Keeping the
/// label set as a small `Vec` of static-keyed pairs avoids a `HashMap`
/// allocation per point in the pure mappers; the collector lifts it into the
/// wire `HashMap` at publish time.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    pub metric: String,
    pub value: TelemetryValue,
    pub labels: Vec<(&'static str, String)>,
}

impl Metric {
    /// A gauge metric with no labels.
    pub fn gauge(metric: impl Into<String>, value: f64) -> Self {
        Self {
            metric: metric.into(),
            value: TelemetryValue::Gauge(value),
            labels: Vec::new(),
        }
    }

    /// A counter metric with no labels.
    pub fn counter(metric: impl Into<String>, value: u64) -> Self {
        Self {
            metric: metric.into(),
            value: TelemetryValue::Counter(value),
            labels: Vec::new(),
        }
    }

    /// Attach a label (builder style).
    pub fn label(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.labels.push((key, value.into()));
        self
    }
}

/// Sanitize a string for use in a Zenoh key expression: collapse runs of
/// reserved characters into a single `_`, then trim leading/trailing `_`.
pub fn sanitize_key(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '/' | ' ' | '#' | '?' | '*' => {
                if !result.ends_with('_') && !result.is_empty() {
                    result.push('_');
                }
            }
            _ => result.push(c),
        }
    }
    result.trim_matches('_').to_string()
}

// ===========================================================================
// A. Pressure Stall Information (PSI)
// ===========================================================================

/// One PSI line (`some` or `full`): the rolling stall percentages plus the
/// cumulative stall time in microseconds. `total` is monotonic — the consumer
/// derives a rate from it rather than trusting only the rolling averages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PressureSample {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_us: u64,
}

/// A full snapshot of `/proc/pressure/{cpu,memory,io}`. `cpu` has only a `some`
/// line; memory and io have both `some` and `full`. `Option` so a missing
/// resource (older kernel / partial PSI) is skipped, never emitted as zeros.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PsiSample {
    pub cpu_some: Option<PressureSample>,
    pub memory_some: Option<PressureSample>,
    pub memory_full: Option<PressureSample>,
    pub io_some: Option<PressureSample>,
    pub io_full: Option<PressureSample>,
}

/// Map a PSI snapshot to wire metrics: `pressure/<res>/<scope>_avg{10,60,300}`
/// gauges (percent) plus a `pressure/<res>/<scope>_total_us` counter.
pub fn map_pressure(psi: &PsiSample) -> Vec<Metric> {
    let mut out = Vec::new();
    let mut emit = |res: &'static str, scope: &'static str, p: &PressureSample| {
        for (suffix, avg) in [
            ("avg10", p.avg10),
            ("avg60", p.avg60),
            ("avg300", p.avg300),
        ] {
            out.push(
                Metric::gauge(format!("pressure/{res}/{scope}_{suffix}"), avg)
                    .label("resource", res)
                    .label("scope", scope),
            );
        }
        out.push(
            Metric::counter(format!("pressure/{res}/{scope}_total_us"), p.total_us)
                .label("resource", res)
                .label("scope", scope),
        );
    };

    if let Some(p) = &psi.cpu_some {
        emit("cpu", "some", p);
    }
    if let Some(p) = &psi.memory_some {
        emit("memory", "some", p);
    }
    if let Some(p) = &psi.memory_full {
        emit("memory", "full", p);
    }
    if let Some(p) = &psi.io_some {
        emit("io", "some", p);
    }
    if let Some(p) = &psi.io_full {
        emit("io", "full", p);
    }
    out
}

// ===========================================================================
// B. vmstat saturation allowlist + /proc/stat derivatives
// ===========================================================================

/// Saturation-relevant subset of `/proc/vmstat` (node_exporter's allowlist:
/// `^(oom_kill|pgpg|pswp|pg.*fault)`). `procfs` 0.17 has no vmstat module, so
/// this is parsed from the flat `key value` file. Fields are `Option` so an
/// absent key is skipped rather than reported as a misleading zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VmStat {
    /// `oom_kill` — the canonical memory-exhaustion failure event.
    pub oom_kill: Option<u64>,
    /// `pgmajfault` — major faults: working set exceeds RAM (saturation).
    pub pgmajfault: Option<u64>,
    /// `pgfault` — all minor+major faults.
    pub pgfault: Option<u64>,
    /// `pswpin` — pages swapped in (swap thrash).
    pub pswpin: Option<u64>,
    /// `pswpout` — pages swapped out (swap thrash).
    pub pswpout: Option<u64>,
    /// `pgpgin` — blocks paged in from disk.
    pub pgpgin: Option<u64>,
    /// `pgpgout` — blocks paged out to disk.
    pub pgpgout: Option<u64>,
}

/// Parse the flat `key value` `/proc/vmstat` file into the allowlisted subset.
/// Unknown keys are ignored; malformed values are skipped.
pub fn parse_vmstat(content: &str) -> VmStat {
    let mut vm = VmStat::default();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(val)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(v) = val.parse::<u64>() else {
            continue;
        };
        match key {
            "oom_kill" => vm.oom_kill = Some(v),
            "pgmajfault" => vm.pgmajfault = Some(v),
            "pgfault" => vm.pgfault = Some(v),
            "pswpin" => vm.pswpin = Some(v),
            "pswpout" => vm.pswpout = Some(v),
            "pgpgin" => vm.pgpgin = Some(v),
            "pgpgout" => vm.pgpgout = Some(v),
            _ => {}
        }
    }
    vm
}

/// Map the vmstat allowlist to cumulative counters. Only present fields are
/// emitted (graceful degradation — no zero spam for keys this kernel lacks).
pub fn map_vmstat(vm: &VmStat) -> Vec<Metric> {
    let mut out = Vec::new();
    if let Some(v) = vm.oom_kill {
        out.push(Metric::counter("memory/oom_kills_total", v));
    }
    if let Some(v) = vm.pgmajfault {
        out.push(Metric::counter("memory/page_faults_major_total", v));
    }
    if let Some(v) = vm.pgfault {
        out.push(Metric::counter("memory/page_faults_total", v));
    }
    if let Some(v) = vm.pswpin {
        out.push(Metric::counter("memory/paging_in_total", v));
    }
    if let Some(v) = vm.pswpout {
        out.push(Metric::counter("memory/paging_out_total", v));
    }
    if let Some(v) = vm.pgpgin {
        out.push(Metric::counter("memory/pgpgin_total", v));
    }
    if let Some(v) = vm.pgpgout {
        out.push(Metric::counter("memory/pgpgout_total", v));
    }
    out
}

/// `/proc/stat` derivatives exposed by the typed `procfs::KernelStats`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KernelDerivatives {
    /// `ctxt` — total context switches since boot (scheduler thrash).
    pub context_switches: u64,
    /// `processes` — total forks since boot (churn / fork-bomb).
    pub forks: u64,
    /// `procs_running` — current run-queue depth.
    pub procs_running: Option<u32>,
    /// `procs_blocked` — processes blocked on I/O.
    pub procs_blocked: Option<u32>,
}

/// Map `/proc/stat` derivatives: cumulative counters for ctxt/forks, gauges for
/// the instantaneous run-queue / blocked counts.
pub fn map_kernel_derivatives(k: &KernelDerivatives) -> Vec<Metric> {
    let mut out = vec![
        Metric::counter("system/context_switches_total", k.context_switches),
        Metric::counter("system/forks_total", k.forks),
    ];
    if let Some(r) = k.procs_running {
        out.push(Metric::gauge("system/procs_running", r as f64));
    }
    if let Some(b) = k.procs_blocked {
        out.push(Metric::gauge("system/procs_blocked", b as f64));
    }
    out
}

// ===========================================================================
// C. FD + inode saturation ceilings
// ===========================================================================

/// File-descriptor table occupancy from `/proc/sys/fs/file-nr`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FdStat {
    /// Allocated-minus-free file handles currently in use.
    pub used: u64,
    /// System-wide ceiling (`fs.file-max`).
    pub max: u64,
}

/// Parse `/proc/sys/fs/file-nr` (`<allocated> <free> <max>`). On modern kernels
/// `free` is always 0, so `used == allocated`, but we subtract defensively.
pub fn parse_file_nr(content: &str) -> Option<FdStat> {
    let mut parts = content.split_whitespace();
    let allocated: u64 = parts.next()?.parse().ok()?;
    let free: u64 = parts.next()?.parse().ok()?;
    let max: u64 = parts.next()?.parse().ok()?;
    Some(FdStat {
        used: allocated.saturating_sub(free),
        max,
    })
}

/// Map FD occupancy: used/max gauges plus a used-percent gauge.
pub fn map_fd(fd: &FdStat) -> Vec<Metric> {
    let pct = if fd.max > 0 {
        (fd.used as f64 / fd.max as f64) * 100.0
    } else {
        0.0
    };
    vec![
        Metric::gauge("system/file_descriptors_used", fd.used as f64),
        Metric::gauge("system/file_descriptors_max", fd.max as f64),
        Metric::gauge("system/file_descriptors_used_percent", pct),
    ]
}

/// Per-mount inode occupancy from `statvfs()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InodeStat {
    pub mount: String,
    pub fs_type: String,
    pub total: u64,
    pub free: u64,
    pub used: u64,
}

/// Map per-mount inode stats: total/used/free gauges + used-percent, keyed by
/// the sanitized mount path with `mount`/`fs_type` labels.
pub fn map_inodes(stats: &[InodeStat]) -> Vec<Metric> {
    let mut out = Vec::new();
    for s in stats {
        let key = sanitize_key(&s.mount);
        let pct = if s.total > 0 {
            (s.used as f64 / s.total as f64) * 100.0
        } else {
            0.0
        };
        let label = |m: Metric| {
            m.label("mount", s.mount.clone())
                .label("fs_type", s.fs_type.clone())
        };
        out.push(label(Metric::gauge(
            format!("disk/{key}/inodes_total"),
            s.total as f64,
        )));
        out.push(label(Metric::gauge(
            format!("disk/{key}/inodes_used"),
            s.used as f64,
        )));
        out.push(label(Metric::gauge(
            format!("disk/{key}/inodes_free"),
            s.free as f64,
        )));
        out.push(label(Metric::gauge(
            format!("disk/{key}/inode_used_percent"),
            pct,
        )));
    }
    out
}

// ===========================================================================
// D. NIC drops + richer /proc/net/dev
// ===========================================================================

/// Per-interface saturation/error counters from `/proc/net/dev` — the drop /
/// fifo / frame / collision fields the `sysinfo` counters omit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetDevStat {
    pub iface: String,
    pub rx_dropped: u64,
    pub rx_fifo: u64,
    pub rx_frame: u64,
    pub multicast: u64,
    pub tx_dropped: u64,
    pub tx_fifo: u64,
    pub tx_colls: u64,
    pub tx_carrier: u64,
}

/// Parse `/proc/net/dev`. The first two lines are headers. Each data line is
/// `iface: <16 whitespace-separated counters>`:
/// rx: bytes packets errs drop fifo frame compressed multicast
/// tx: bytes packets errs drop fifo colls carrier compressed
pub fn parse_net_dev(content: &str) -> Vec<NetDevStat> {
    let mut out = Vec::new();
    for line in content.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = name.trim().to_string();
        if iface.is_empty() {
            continue;
        }
        let cols: Vec<u64> = rest
            .split_whitespace()
            .map(|c| c.parse::<u64>().unwrap_or(0))
            .collect();
        // Need all 16 columns to trust the drop/fifo offsets.
        if cols.len() < 16 {
            continue;
        }
        out.push(NetDevStat {
            iface,
            rx_dropped: cols[3],
            rx_fifo: cols[4],
            rx_frame: cols[5],
            multicast: cols[7],
            tx_dropped: cols[11],
            tx_fifo: cols[12],
            tx_colls: cols[13],
            tx_carrier: cols[14],
        });
    }
    out
}

/// Map per-interface drop/fifo/frame/collision counters under
/// `network/<iface>/...`. `iface` is sanitized for the key; the original name
/// is preserved in the `interface` label.
pub fn map_net_dev(stats: &[NetDevStat]) -> Vec<Metric> {
    let mut out = Vec::new();
    for s in stats {
        let key = sanitize_key(&s.iface);
        let label = |m: Metric| m.label("interface", s.iface.clone());
        for (suffix, val) in [
            ("rx_dropped", s.rx_dropped),
            ("rx_fifo", s.rx_fifo),
            ("rx_frame", s.rx_frame),
            ("multicast", s.multicast),
            ("tx_dropped", s.tx_dropped),
            ("tx_fifo", s.tx_fifo),
            ("tx_colls", s.tx_colls),
            ("tx_carrier", s.tx_carrier),
        ] {
            out.push(label(Metric::counter(
                format!("network/{key}/{suffix}"),
                val,
            )));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_key() {
        assert_eq!(sanitize_key("/"), "");
        assert_eq!(sanitize_key("/home"), "home");
        assert_eq!(sanitize_key("/home/user"), "home_user");
        assert_eq!(sanitize_key("eth0"), "eth0");
        assert_eq!(sanitize_key("my interface"), "my_interface");
    }

    #[test]
    fn test_map_pressure_emits_avgs_and_total() {
        let psi = PsiSample {
            cpu_some: Some(PressureSample {
                avg10: 1.5,
                avg60: 0.5,
                avg300: 0.1,
                total_us: 12345,
            }),
            memory_full: Some(PressureSample {
                avg10: 9.0,
                avg60: 4.0,
                avg300: 1.0,
                total_us: 999,
            }),
            ..Default::default()
        };
        let m = map_pressure(&psi);
        // cpu: 3 avgs + 1 total; memory full: 3 avgs + 1 total => 8
        assert_eq!(m.len(), 8);
        let cpu_total = m
            .iter()
            .find(|x| x.metric == "pressure/cpu/some_total_us")
            .unwrap();
        assert_eq!(cpu_total.value, TelemetryValue::Counter(12345));
        let cpu_avg10 = m
            .iter()
            .find(|x| x.metric == "pressure/cpu/some_avg10")
            .unwrap();
        assert_eq!(cpu_avg10.value, TelemetryValue::Gauge(1.5));
        assert!(m.iter().any(|x| x.metric == "pressure/memory/full_avg300"));
        // Absent resources are not emitted.
        assert!(!m.iter().any(|x| x.metric.starts_with("pressure/io/")));
    }

    #[test]
    fn test_parse_vmstat_fixture() {
        let fixture = "nr_free_pages 100\n\
                       pgpgin 5000\n\
                       pgpgout 6000\n\
                       pswpin 12\n\
                       pswpout 34\n\
                       pgfault 700000\n\
                       pgmajfault 250\n\
                       oom_kill 3\n\
                       nr_dirty 7\n";
        let vm = parse_vmstat(fixture);
        assert_eq!(vm.oom_kill, Some(3));
        assert_eq!(vm.pgmajfault, Some(250));
        assert_eq!(vm.pgfault, Some(700000));
        assert_eq!(vm.pswpin, Some(12));
        assert_eq!(vm.pswpout, Some(34));
        assert_eq!(vm.pgpgin, Some(5000));
        assert_eq!(vm.pgpgout, Some(6000));
    }

    #[test]
    fn test_parse_vmstat_missing_keys() {
        // A kernel without oom_kill / swap accounting.
        let vm = parse_vmstat("pgfault 10\npgmajfault 2\n");
        assert_eq!(vm.oom_kill, None);
        assert_eq!(vm.pswpin, None);
        let m = map_vmstat(&vm);
        // Only present keys are mapped.
        assert!(m.iter().any(|x| x.metric == "memory/page_faults_total"));
        assert!(!m.iter().any(|x| x.metric == "memory/oom_kills_total"));
        assert!(!m.iter().any(|x| x.metric == "memory/paging_in_total"));
    }

    #[test]
    fn test_map_vmstat_counters() {
        let vm = VmStat {
            oom_kill: Some(3),
            pgmajfault: Some(250),
            pswpin: Some(12),
            pswpout: Some(34),
            ..Default::default()
        };
        let m = map_vmstat(&vm);
        let oom = m
            .iter()
            .find(|x| x.metric == "memory/oom_kills_total")
            .unwrap();
        assert_eq!(oom.value, TelemetryValue::Counter(3));
    }

    #[test]
    fn test_map_kernel_derivatives() {
        let k = KernelDerivatives {
            context_switches: 9000,
            forks: 1234,
            procs_running: Some(2),
            procs_blocked: Some(0),
        };
        let m = map_kernel_derivatives(&k);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "system/context_switches_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(9000)
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "system/procs_running")
                .unwrap()
                .value,
            TelemetryValue::Gauge(2.0)
        );
    }

    #[test]
    fn test_kernel_derivatives_skips_absent_gauges() {
        let k = KernelDerivatives {
            context_switches: 1,
            forks: 1,
            procs_running: None,
            procs_blocked: None,
        };
        let m = map_kernel_derivatives(&k);
        assert!(!m.iter().any(|x| x.metric == "system/procs_running"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn test_parse_file_nr() {
        let fd = parse_file_nr("2048\t0\t1572864\n").unwrap();
        assert_eq!(fd.used, 2048);
        assert_eq!(fd.max, 1572864);
        // free is subtracted from allocated.
        let fd2 = parse_file_nr("100 10 1000").unwrap();
        assert_eq!(fd2.used, 90);
    }

    #[test]
    fn test_parse_file_nr_malformed() {
        assert!(parse_file_nr("").is_none());
        assert!(parse_file_nr("1 2").is_none());
        assert!(parse_file_nr("a b c").is_none());
    }

    #[test]
    fn test_map_fd_percent() {
        let m = map_fd(&FdStat {
            used: 50,
            max: 200,
        });
        let pct = m
            .iter()
            .find(|x| x.metric == "system/file_descriptors_used_percent")
            .unwrap();
        assert_eq!(pct.value, TelemetryValue::Gauge(25.0));
    }

    #[test]
    fn test_map_fd_zero_max_no_div0() {
        let m = map_fd(&FdStat { used: 5, max: 0 });
        let pct = m
            .iter()
            .find(|x| x.metric == "system/file_descriptors_used_percent")
            .unwrap();
        assert_eq!(pct.value, TelemetryValue::Gauge(0.0));
    }

    #[test]
    fn test_map_inodes() {
        let stats = vec![InodeStat {
            mount: "/home".to_string(),
            fs_type: "ext4".to_string(),
            total: 1000,
            free: 750,
            used: 250,
        }];
        let m = map_inodes(&stats);
        assert!(m.iter().any(|x| x.metric == "disk/home/inodes_total"));
        let pct = m
            .iter()
            .find(|x| x.metric == "disk/home/inode_used_percent")
            .unwrap();
        assert_eq!(pct.value, TelemetryValue::Gauge(25.0));
        // labels preserve the original mount path.
        assert!(pct.labels.contains(&("mount", "/home".to_string())));
    }

    #[test]
    fn test_parse_net_dev_fixture() {
        // Real-shape header + two interfaces (lo and eth0).
        let fixture = "Inter-|   Receive                                                |  Transmit\n\
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
    lo: 1000      10    0    0    0    0     0          0         1000      10    0    0    0     0     0       0\n\
  eth0: 5000      50    1    2    3    4     0          5         6000      60    7    8    9    10    11       0\n";
        let stats = parse_net_dev(fixture);
        assert_eq!(stats.len(), 2);
        let eth0 = stats.iter().find(|s| s.iface == "eth0").unwrap();
        assert_eq!(eth0.rx_dropped, 2);
        assert_eq!(eth0.rx_fifo, 3);
        assert_eq!(eth0.rx_frame, 4);
        assert_eq!(eth0.multicast, 5);
        assert_eq!(eth0.tx_dropped, 8);
        assert_eq!(eth0.tx_fifo, 9);
        assert_eq!(eth0.tx_colls, 10);
        assert_eq!(eth0.tx_carrier, 11);
    }

    #[test]
    fn test_parse_net_dev_skips_short_lines() {
        let fixture = "h1\nh2\nbroken: 1 2 3\n";
        assert!(parse_net_dev(fixture).is_empty());
    }

    #[test]
    fn test_map_net_dev() {
        let stats = vec![NetDevStat {
            iface: "eth0".to_string(),
            rx_dropped: 2,
            tx_dropped: 8,
            ..Default::default()
        }];
        let m = map_net_dev(&stats);
        let rx = m
            .iter()
            .find(|x| x.metric == "network/eth0/rx_dropped")
            .unwrap();
        assert_eq!(rx.value, TelemetryValue::Counter(2));
        assert!(rx.labels.contains(&("interface", "eth0".to_string())));
    }
}
