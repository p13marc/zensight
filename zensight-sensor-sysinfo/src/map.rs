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
///
/// An input that reduces to the empty string (notably the root mount `"/"`)
/// would produce an empty key chunk, which Zenoh rejects (`disk//total`), so it
/// is mapped to the literal `root`.
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
    let trimmed = result.trim_matches('_');
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.to_string()
    }
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
        for (suffix, avg) in [("avg10", p.avg10), ("avg60", p.avg60), ("avg300", p.avg300)] {
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

// ===========================================================================
// E. cgroup-v2 (container saturation): throttling / OOM / memory / pressure
// ===========================================================================

/// Saturation-relevant cgroup-v2 sample for a single cgroup. Every field is
/// `Option` so a controller that is not enabled for this cgroup (or a kernel
/// without that file) is skipped rather than reported as a misleading zero.
/// The high-signal fields here are *throttling* and *OOM*, not raw usage
/// (cAdvisor dropped these on v2 — we read the files directly).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CgroupSample {
    /// The cgroup path (e.g. `/system.slice/foo.service`), used as the metric
    /// key suffix and a `cgroup` label.
    pub path: String,
    /// `cpu.stat` `nr_throttled` — number of throttled enforcement periods.
    pub cpu_nr_throttled: Option<u64>,
    /// `cpu.stat` `throttled_usec` — total time the cgroup was throttled.
    pub cpu_throttled_usec: Option<u64>,
    /// `memory.current` — current memory usage in bytes.
    pub memory_current: Option<u64>,
    /// `memory.max` — the hard limit in bytes (`None` when set to `max`).
    pub memory_max: Option<u64>,
    /// `memory.events` `oom_kill` — processes OOM-killed in this cgroup.
    pub memory_oom_kills: Option<u64>,
    /// `memory.events` `oom` — times the cgroup hit its limit.
    pub memory_oom: Option<u64>,
    /// `cpu.pressure` `some` PSI line.
    pub cpu_pressure_some: Option<PressureSample>,
    /// `memory.pressure` `some` PSI line.
    pub memory_pressure_some: Option<PressureSample>,
    /// `memory.pressure` `full` PSI line.
    pub memory_pressure_full: Option<PressureSample>,
    /// `io.pressure` `some` PSI line.
    pub io_pressure_some: Option<PressureSample>,
    /// `io.pressure` `full` PSI line.
    pub io_pressure_full: Option<PressureSample>,
}

/// Parse a flat cgroup-v2 `key value` file (e.g. `cpu.stat`, `memory.events`)
/// into a small lookup. Values that do not parse as `u64` are skipped.
pub fn parse_flat_kv(content: &str) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(k), Some(v)) = (parts.next(), parts.next())
            && let Ok(n) = v.parse::<u64>()
        {
            map.insert(k.to_string(), n);
        }
    }
    map
}

/// Parse a single-value cgroup file (e.g. `memory.current`, `memory.max`).
/// The literal `max` (no limit) yields `None`; a numeric value yields `Some`.
pub fn parse_cgroup_scalar(content: &str) -> Option<u64> {
    let t = content.trim();
    if t == "max" {
        return None;
    }
    t.parse::<u64>().ok()
}

/// Parse a kernel PSI-format file (`/proc/pressure/*` or a cgroup `*.pressure`).
/// Lines look like `some avg10=0.00 avg60=0.00 avg300=0.00 total=12345`.
/// Returns `(some, full)` where each is present only if its line exists.
pub fn parse_pressure_file(content: &str) -> (Option<PressureSample>, Option<PressureSample>) {
    let mut some = None;
    let mut full = None;
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let Some(scope) = parts.next() else { continue };
        let mut p = PressureSample::default();
        for kv in parts {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "avg10" => p.avg10 = v.parse().unwrap_or(0.0),
                "avg60" => p.avg60 = v.parse().unwrap_or(0.0),
                "avg300" => p.avg300 = v.parse().unwrap_or(0.0),
                "total" => p.total_us = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        match scope {
            "some" => some = Some(p),
            "full" => full = Some(p),
            _ => {}
        }
    }
    (some, full)
}

/// Map a cgroup-v2 sample to wire metrics under `cgroup/...`, keyed by resource
/// and carrying the cgroup `path` as a `cgroup` label. Only present fields are
/// emitted (graceful degradation per controller availability).
pub fn map_cgroup(c: &CgroupSample) -> Vec<Metric> {
    let mut out = Vec::new();
    let label = |m: Metric| m.label("cgroup", c.path.clone());

    if let Some(v) = c.cpu_nr_throttled {
        out.push(label(Metric::counter("cgroup/cpu/nr_throttled", v)));
    }
    if let Some(v) = c.cpu_throttled_usec {
        out.push(label(Metric::counter("cgroup/cpu/throttled_usec", v)));
    }
    if let Some(v) = c.memory_current {
        out.push(label(Metric::gauge("cgroup/memory/current", v as f64)));
    }
    if let Some(v) = c.memory_max {
        out.push(label(Metric::gauge("cgroup/memory/max", v as f64)));
    }
    // A used-percent against the limit is the actionable container signal.
    if let (Some(cur), Some(max)) = (c.memory_current, c.memory_max)
        && max > 0
    {
        out.push(label(Metric::gauge(
            "cgroup/memory/used_percent",
            (cur as f64 / max as f64) * 100.0,
        )));
    }
    if let Some(v) = c.memory_oom_kills {
        out.push(label(Metric::counter("cgroup/memory/oom_kills_total", v)));
    }
    if let Some(v) = c.memory_oom {
        out.push(label(Metric::counter("cgroup/memory/oom_total", v)));
    }

    let mut emit_psi = |res: &'static str, scope: &'static str, p: &PressureSample| {
        out.push(label(
            Metric::gauge(format!("cgroup/{res}/pressure/{scope}_avg10"), p.avg10)
                .label("resource", res)
                .label("scope", scope),
        ));
        out.push(label(
            Metric::counter(
                format!("cgroup/{res}/pressure/{scope}_total_us"),
                p.total_us,
            )
            .label("resource", res)
            .label("scope", scope),
        ));
    };
    if let Some(p) = &c.cpu_pressure_some {
        emit_psi("cpu", "some", p);
    }
    if let Some(p) = &c.memory_pressure_some {
        emit_psi("memory", "some", p);
    }
    if let Some(p) = &c.memory_pressure_full {
        emit_psi("memory", "full", p);
    }
    if let Some(p) = &c.io_pressure_some {
        emit_psi("io", "some", p);
    }
    if let Some(p) = &c.io_pressure_full {
        emit_psi("io", "full", p);
    }
    out
}

// ===========================================================================
// G. Thermal / power depth (RAPL, fans, battery, entropy)
// ===========================================================================

/// A RAPL energy domain (`/sys/class/powercap/<zone>`). `energy_uj` is the
/// monotonic energy counter in microjoules; the collector derives instantaneous
/// watts from the delta across ticks. `max_energy_uj` is the wraparound range.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RaplDomain {
    pub zone: String,
    pub name: String,
    pub energy_uj: u64,
    pub max_energy_uj: Option<u64>,
}

/// A hwmon fan reading (`fan*_input`, RPM).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FanReading {
    pub chip: String,
    pub label: String,
    pub rpm: f64,
}

/// A battery / power-supply reading (`/sys/class/power_supply/<name>`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryReading {
    pub name: String,
    /// `capacity` percent (0..100), if present.
    pub capacity: Option<f64>,
    /// `status` string (Charging / Discharging / Full / ...), if present.
    pub status: Option<String>,
}

/// A bundle of the thermal/power-depth readings (§G). `entropy_avail` is the
/// kernel CSPRNG pool fill (`/proc/sys/kernel/random/entropy_avail`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PowerSample {
    /// Per-RAPL-domain instantaneous watts (already rate-derived by the
    /// collector from the energy counter delta).
    pub rapl_watts: Vec<(String, String, f64)>,
    pub fans: Vec<FanReading>,
    pub batteries: Vec<BatteryReading>,
    pub entropy_avail: Option<u64>,
}

/// Derive watts from two RAPL energy readings taken `interval_secs` apart,
/// handling the counter wrap at `max_energy_uj`. Returns `None` if the interval
/// is non-positive or the reading is the first for this domain.
pub fn rapl_watts(
    prev_uj: u64,
    cur_uj: u64,
    max_uj: Option<u64>,
    interval_secs: f64,
) -> Option<f64> {
    if interval_secs <= 0.0 {
        return None;
    }
    let delta_uj = if cur_uj >= prev_uj {
        cur_uj - prev_uj
    } else {
        // Counter wrapped.
        match max_uj {
            Some(m) if m > 0 => (m - prev_uj).saturating_add(cur_uj),
            _ => return None,
        }
    };
    // microjoules / 1e6 = joules; joules / seconds = watts.
    Some((delta_uj as f64 / 1_000_000.0) / interval_secs)
}

/// Map the thermal/power-depth bundle to wire metrics under `power/...`,
/// `sensors/<chip>/<fan>/rpm`, `battery/<name>/...`, `system/entropy_avail`.
pub fn map_power(s: &PowerSample) -> Vec<Metric> {
    let mut out = Vec::new();
    for (zone, name, watts) in &s.rapl_watts {
        out.push(
            Metric::gauge(format!("power/rapl/{}/watts", sanitize_key(zone)), *watts)
                .label("zone", zone.clone())
                .label("name", name.clone()),
        );
    }
    for f in &s.fans {
        out.push(
            Metric::gauge(
                format!(
                    "sensors/{}/{}/rpm",
                    sanitize_key(&f.chip),
                    sanitize_key(&f.label)
                ),
                f.rpm,
            )
            .label("chip", f.chip.clone())
            .label("label", f.label.clone()),
        );
    }
    for b in &s.batteries {
        let key = sanitize_key(&b.name);
        if let Some(cap) = b.capacity {
            out.push(
                Metric::gauge(format!("battery/{key}/capacity"), cap).label("name", b.name.clone()),
            );
        }
        if let Some(status) = &b.status {
            out.push(
                Metric {
                    metric: format!("battery/{key}/status"),
                    value: TelemetryValue::Text(status.clone()),
                    labels: Vec::new(),
                }
                .label("name", b.name.clone()),
            );
        }
    }
    if let Some(e) = s.entropy_avail {
        out.push(Metric::gauge("system/entropy_avail", e as f64));
    }
    out
}

// ===========================================================================
// F. Per-process detail query channel (selector parsing)
// ===========================================================================

/// How to rank processes for the `@/query/processes` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessSort {
    /// By CPU usage, descending (default).
    #[default]
    Cpu,
    /// By resident memory, descending.
    Mem,
    /// By total disk I/O (read+write bytes), descending.
    Io,
}

/// Parsed `@/query/processes?sort=cpu|mem|io&top=N` selector. Bounds `top` so a
/// caller cannot request an unbounded firehose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessSelector {
    pub sort: ProcessSort,
    pub top: usize,
}

impl Default for ProcessSelector {
    fn default() -> Self {
        Self {
            sort: ProcessSort::Cpu,
            top: 20,
        }
    }
}

impl ProcessSelector {
    /// Maximum rows a single query may request (cardinality discipline, P2).
    pub const MAX_TOP: usize = 200;

    /// Parse the Zenoh query parameter string (`sort=cpu&top=10`). Unknown keys
    /// are ignored; bad values fall back to defaults; `top` is clamped to
    /// `1..=MAX_TOP`.
    pub fn parse(params: &str) -> Self {
        let mut sel = Self::default();
        for pair in params.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            match k.trim() {
                "sort" => {
                    sel.sort = match v.trim().to_ascii_lowercase().as_str() {
                        "mem" | "memory" => ProcessSort::Mem,
                        "io" => ProcessSort::Io,
                        _ => ProcessSort::Cpu,
                    }
                }
                "top" => {
                    if let Ok(n) = v.trim().parse::<usize>() {
                        sel.top = n.clamp(1, Self::MAX_TOP);
                    }
                }
                _ => {}
            }
        }
        sel
    }
}

// ===========================================================================
// G. Derived saturation gauges: disk %util / queue depth, memory pressure.
// ===========================================================================

/// Disk saturation gauges derived from a poll-to-poll `/proc/diskstats` delta.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DiskSaturation {
    /// Fraction of wall-clock time the device had I/O in flight, `0..=100`.
    pub util_percent: f64,
    /// Time-weighted average request-queue length (iostat `aqu-sz`).
    pub queue_depth: f64,
}

/// `%util` = (io_time delta ms / interval ms) * 100, clamped to `0..=100`.
///
/// `io_time` is diskstats field 10 (`io_ticks`, ms the device had any I/O in
/// flight). A non-positive interval (first tick / clock skew) yields 0.
pub fn disk_util_percent(io_time_delta_ms: u64, interval_ms: f64) -> f64 {
    if interval_ms <= 0.0 {
        return 0.0;
    }
    ((io_time_delta_ms as f64 / interval_ms) * 100.0).clamp(0.0, 100.0)
}

/// Average queue depth = weighted_io_time delta / io_time delta (both ms).
///
/// `weighted_io_time` is diskstats field 11 (the time-weighted I/O queue
/// length). Guards division by zero: an idle device (`io_time` delta == 0)
/// reports 0 rather than `NaN`/`inf`.
pub fn disk_queue_depth(weighted_delta_ms: u64, io_time_delta_ms: u64) -> f64 {
    if io_time_delta_ms == 0 {
        return 0.0;
    }
    weighted_delta_ms as f64 / io_time_delta_ms as f64
}

/// Both disk-saturation gauges from one diskstats delta + the poll interval.
pub fn disk_saturation(
    io_time_delta_ms: u64,
    weighted_delta_ms: u64,
    interval_ms: f64,
) -> DiskSaturation {
    DiskSaturation {
        util_percent: disk_util_percent(io_time_delta_ms, interval_ms),
        queue_depth: disk_queue_depth(weighted_delta_ms, io_time_delta_ms),
    }
}

/// MemAvailable-based memory usage percent: `(total - available) / total * 100`,
/// clamped to `0..=100`.
///
/// Unlike a `used`-based figure (which counts reclaimable page cache) this
/// tracks real memory pressure: `available` is the kernel's `MemAvailable`
/// estimate of memory obtainable without swapping. Zero `total` yields 0.
pub fn mem_usage_percent(total: u64, available: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let used = total.saturating_sub(available);
    ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0)
}

// ===========================================================================
// H. USE-completeness collectors (#98): netstat/sockstat, softnet, schedstat,
//    conntrack, edac, mdadm. Pure parsers + map fns; the Linux reads live in
//    `linux.rs`. Every field/array degrades to "skip" rather than fake zeros.
// ===========================================================================

/// Parse a `/proc/net/{snmp,netstat}`-style file. These store stats as
/// alternating header/value line pairs sharing a leading `Prefix:` tag, e.g.
/// ```text
/// Tcp: RtoAlgorithm RtoMin ... RetransSegs InErrs OutRsts InCsumErrors
/// Tcp: 1 200 ... 9905 107 2369 0
/// ```
/// Returns a map keyed `Prefix:Field` -> value. A line is treated as the value
/// row when its first column parses as an integer (field names never do); value
/// columns that are not non-negative integers (e.g. `MaxConn -1`) are dropped.
pub fn parse_proc_net_stats(content: &str) -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
    let mut headers: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for line in content.lines() {
        let Some((prefix, rest)) = line.split_once(':') else {
            continue;
        };
        let prefix = prefix.trim();
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let is_value = tokens
            .first()
            .map(|t| t.parse::<i64>().is_ok())
            .unwrap_or(false);
        if is_value {
            if let Some(names) = headers.get(prefix) {
                for (name, val) in names.iter().zip(tokens.iter()) {
                    if let Ok(v) = val.parse::<u64>() {
                        out.insert(format!("{prefix}:{name}"), v);
                    }
                }
            }
        } else {
            headers.insert(
                prefix.to_string(),
                tokens.iter().map(|s| s.to_string()).collect(),
            );
        }
    }
    out
}

/// TCP retransmit / listen-queue-overflow saturation+error counters from
/// `/proc/net/snmp` (`Tcp:RetransSegs`) and `/proc/net/netstat`
/// (`TcpExt:ListenOverflows`, `TcpExt:ListenDrops`). Every field is `Option`
/// so a kernel that omits it is skipped rather than reported as zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetstatSample {
    pub tcp_retrans_segs: Option<u64>,
    pub listen_overflows: Option<u64>,
    pub listen_drops: Option<u64>,
}

/// Parse the `snmp` + `netstat` file contents into the saturation subset.
pub fn parse_netstat(snmp: &str, netstat: &str) -> NetstatSample {
    let s = parse_proc_net_stats(snmp);
    let n = parse_proc_net_stats(netstat);
    NetstatSample {
        tcp_retrans_segs: s.get("Tcp:RetransSegs").copied(),
        listen_overflows: n.get("TcpExt:ListenOverflows").copied(),
        listen_drops: n.get("TcpExt:ListenDrops").copied(),
    }
}

/// Map TCP retransmit / listen-overflow counters under `network/tcp/...`.
pub fn map_netstat(s: &NetstatSample) -> Vec<Metric> {
    let mut out = Vec::new();
    if let Some(v) = s.tcp_retrans_segs {
        out.push(Metric::counter("network/tcp/retrans_segs_total", v));
    }
    if let Some(v) = s.listen_overflows {
        out.push(Metric::counter("network/tcp/listen_overflows_total", v));
    }
    if let Some(v) = s.listen_drops {
        out.push(Metric::counter("network/tcp/listen_drops_total", v));
    }
    out
}

/// Sockets-in-use + TCP memory-pressure gauges from `/proc/net/sockstat`. These
/// are instantaneous occupancy figures (gauges, not counters). `tcp_mem_pages`
/// is the `TCP: ... mem` column in kernel pages.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SockstatSample {
    pub sockets_used: Option<u64>,
    pub tcp_inuse: Option<u64>,
    pub tcp_mem_pages: Option<u64>,
    pub udp_inuse: Option<u64>,
}

/// Parse `/proc/net/sockstat`. Lines are `Prefix: key val key val ...`, e.g.
/// `sockets: used 1107` / `TCP: inuse 13 orphan 0 tw 2 alloc 17 mem 750`.
pub fn parse_sockstat(content: &str) -> SockstatSample {
    let mut sample = SockstatSample::default();
    for line in content.lines() {
        let Some((prefix, rest)) = line.split_once(':') else {
            continue;
        };
        let toks: Vec<&str> = rest.split_whitespace().collect();
        let mut kv = std::collections::HashMap::new();
        let mut i = 0;
        while i + 1 < toks.len() {
            if let Ok(v) = toks[i + 1].parse::<u64>() {
                kv.insert(toks[i], v);
            }
            i += 2;
        }
        match prefix.trim() {
            "sockets" => sample.sockets_used = kv.get("used").copied(),
            "TCP" => {
                sample.tcp_inuse = kv.get("inuse").copied();
                sample.tcp_mem_pages = kv.get("mem").copied();
            }
            "UDP" => sample.udp_inuse = kv.get("inuse").copied(),
            _ => {}
        }
    }
    sample
}

/// Map sockstat occupancy under `network/sockets/...` (present fields only).
pub fn map_sockstat(s: &SockstatSample) -> Vec<Metric> {
    let mut out = Vec::new();
    if let Some(v) = s.sockets_used {
        out.push(Metric::gauge("network/sockets/used", v as f64));
    }
    if let Some(v) = s.tcp_inuse {
        out.push(Metric::gauge("network/sockets/tcp_inuse", v as f64));
    }
    if let Some(v) = s.tcp_mem_pages {
        out.push(Metric::gauge("network/sockets/tcp_mem_pages", v as f64));
    }
    if let Some(v) = s.udp_inuse {
        out.push(Metric::gauge("network/sockets/udp_inuse", v as f64));
    }
    out
}

/// Softnet (NIC→kernel backpressure) totals from `/proc/net/softnet_stat`,
/// summed across the per-CPU rows. Columns are **hex**: col0 = packets
/// processed, col1 = packets dropped (backlog full), col2 = times the softirq
/// ran out of budget/time (`time_squeeze`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SoftnetSample {
    pub processed: u64,
    pub dropped: u64,
    pub squeezed: u64,
}

/// Parse `/proc/net/softnet_stat` (one hex-column row per CPU), summing the
/// processed/dropped/squeezed columns. Returns `None` if no valid row is found.
pub fn parse_softnet(content: &str) -> Option<SoftnetSample> {
    let mut s = SoftnetSample::default();
    let mut any = false;
    for line in content.lines() {
        let cols: Vec<u64> = line
            .split_whitespace()
            .map(|c| u64::from_str_radix(c, 16).unwrap_or(0))
            .collect();
        if cols.len() < 3 {
            continue;
        }
        s.processed = s.processed.saturating_add(cols[0]);
        s.dropped = s.dropped.saturating_add(cols[1]);
        s.squeezed = s.squeezed.saturating_add(cols[2]);
        any = true;
    }
    if any { Some(s) } else { None }
}

/// Map softnet totals under `network/softnet/...` as cumulative counters.
pub fn map_softnet(s: &SoftnetSample) -> Vec<Metric> {
    vec![
        Metric::counter("network/softnet/processed_total", s.processed),
        Metric::counter("network/softnet/dropped_total", s.dropped),
        Metric::counter("network/softnet/squeezed_total", s.squeezed),
    ]
}

/// Per-CPU scheduler run-delay (the canonical CPU saturation signal) from
/// `/proc/schedstat`. `total_run_delay_ns` sums the per-CPU cumulative
/// nanoseconds tasks spent waiting on a runqueue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchedstatSample {
    /// `(cpu_index, run_delay_ns)` per CPU.
    pub per_cpu: Vec<(u32, u64)>,
    pub total_run_delay_ns: u64,
}

/// Parse `/proc/schedstat`. Only `cpu<N>` lines carry per-CPU stats; the
/// scheduling-latency block is the last three of nine fields, and **run-delay
/// is the 8th statistic** (0-based index 7 of the values after `cpu<N>`). This
/// layout has been stable across schedstat versions 15-17. Returns `None` when
/// no `cpu<N>` line is present.
pub fn parse_schedstat(content: &str) -> Option<SchedstatSample> {
    let mut per_cpu = Vec::new();
    let mut total: u64 = 0;
    for line in content.lines() {
        let mut toks = line.split_whitespace();
        let Some(name) = toks.next() else { continue };
        let Some(num) = name.strip_prefix("cpu") else {
            continue;
        };
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(idx) = num.parse::<u32>() else {
            continue;
        };
        let vals: Vec<u64> = toks.map(|t| t.parse::<u64>().unwrap_or(0)).collect();
        if vals.len() < 8 {
            continue;
        }
        let run_delay = vals[7];
        total = total.saturating_add(run_delay);
        per_cpu.push((idx, run_delay));
    }
    if per_cpu.is_empty() {
        None
    } else {
        Some(SchedstatSample {
            per_cpu,
            total_run_delay_ns: total,
        })
    }
}

/// Map run-delay under `cpu/schedstat/run_delay_ns_total` (host total) plus a
/// per-CPU `cpu<N>/schedstat/run_delay_ns_total` counter carrying a `core`
/// label. Cumulative ns counters — the consumer derives the ns/s rate.
pub fn map_schedstat(s: &SchedstatSample) -> Vec<Metric> {
    let mut out = vec![Metric::counter(
        "cpu/schedstat/run_delay_ns_total",
        s.total_run_delay_ns,
    )];
    for (cpu, ns) in &s.per_cpu {
        out.push(
            Metric::counter(format!("cpu{cpu}/schedstat/run_delay_ns_total"), *ns)
                .label("core", cpu.to_string()),
        );
    }
    out
}

/// Conntrack table fill from `nf_conntrack_count` (+ `nf_conntrack_max` when
/// readable). A near-full table is a silent firewall outage.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConntrackSample {
    pub count: u64,
    pub max: Option<u64>,
}

/// Parse the conntrack count file (required) and the optional max file (each a
/// single integer). Returns `None` if the count does not parse.
pub fn parse_conntrack(count: &str, max: Option<&str>) -> Option<ConntrackSample> {
    let count = count.trim().parse::<u64>().ok()?;
    let max = max.and_then(|m| m.trim().parse::<u64>().ok());
    Some(ConntrackSample { count, max })
}

/// Map conntrack under `network/conntrack/{count,max,utilization_percent}`.
/// `max`/`utilization_percent` are emitted only when the max file was readable.
pub fn map_conntrack(s: &ConntrackSample) -> Vec<Metric> {
    let mut out = vec![Metric::gauge("network/conntrack/count", s.count as f64)];
    if let Some(max) = s.max {
        out.push(Metric::gauge("network/conntrack/max", max as f64));
        if max > 0 {
            out.push(Metric::gauge(
                "network/conntrack/utilization_percent",
                (s.count as f64 / max as f64) * 100.0,
            ));
        }
    }
    out
}

/// ECC error counts for one memory controller (`/sys/devices/system/edac/mc/
/// mc<N>/{ce_count,ue_count}`). Correctable errors are a wear signal;
/// uncorrectable errors are imminent data corruption.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdacSample {
    pub controller: String,
    pub ce: u64,
    pub ue: u64,
}

/// Map per-controller ECC counts under `memory/edac/<mc>/{correctable_total,
/// uncorrectable_total}` with a `controller` label.
pub fn map_edac(samples: &[EdacSample]) -> Vec<Metric> {
    let mut out = Vec::new();
    for s in samples {
        let key = sanitize_key(&s.controller);
        let label = |m: Metric| m.label("controller", s.controller.clone());
        out.push(label(Metric::counter(
            format!("memory/edac/{key}/correctable_total"),
            s.ce,
        )));
        out.push(label(Metric::counter(
            format!("memory/edac/{key}/uncorrectable_total"),
            s.ue,
        )));
    }
    out
}

/// One software-RAID array parsed from `/proc/mdstat`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MdArray {
    /// Array name (e.g. `md0`).
    pub name: String,
    /// `true` for an `active` array, `false` for `inactive`.
    pub active: bool,
    /// Configured member count from the `[total/active]` status token.
    pub total_disks: Option<u64>,
    /// Working member count from the `[total/active]` status token.
    pub active_disks: Option<u64>,
    /// Members flagged failed (`(F)`) on the header line.
    pub failed_disks: u64,
    /// `true` if inactive, any member failed, or working < configured.
    pub degraded: bool,
}

/// Parse the `[total/active]` status token (e.g. `[2/1]`). Non-ratio bracket
/// tokens (e.g. `[UU]`) and unbracketed tokens return `None`.
fn parse_md_ratio(tok: &str) -> Option<(u64, u64)> {
    let inner = tok.strip_prefix('[')?.strip_suffix(']')?;
    let (n, m) = inner.split_once('/')?;
    Some((n.trim().parse().ok()?, m.trim().parse().ok()?))
}

/// Parse `/proc/mdstat`. Each array opens with a header line
/// `md0 : active raid1 sdb1[1] sda1[0]` followed by a status line carrying the
/// `[total/active]` ratio and `[UU_]` member map. Redundancy-free arrays
/// (raid0/linear) have no ratio token, so `total/active_disks` stay `None`.
pub fn parse_mdstat(content: &str) -> Vec<MdArray> {
    let mut arrays: Vec<MdArray> = Vec::new();
    let mut cur: Option<MdArray> = None;
    for line in content.lines() {
        // Header line: "<name> : <active|inactive> <personality> <member>[i]..."
        if let Some((name_part, rest)) = line.split_once(" : ") {
            let name = name_part.trim();
            if name.starts_with("md") {
                if let Some(a) = cur.take() {
                    arrays.push(a);
                }
                let active = rest.split_whitespace().next() == Some("active");
                let failed_disks = rest.matches("(F)").count() as u64;
                cur = Some(MdArray {
                    name: name.to_string(),
                    active,
                    failed_disks,
                    ..Default::default()
                });
                continue;
            }
        }
        // Continuation/status line: harvest the first [total/active] ratio.
        if let Some(a) = cur.as_mut()
            && a.total_disks.is_none()
        {
            for tok in line.split_whitespace() {
                if let Some((n, m)) = parse_md_ratio(tok) {
                    a.total_disks = Some(n);
                    a.active_disks = Some(m);
                    break;
                }
            }
        }
    }
    if let Some(a) = cur.take() {
        arrays.push(a);
    }
    for a in &mut arrays {
        let understaffed = matches!((a.total_disks, a.active_disks), (Some(t), Some(ac)) if ac < t);
        a.degraded = !a.active || a.failed_disks > 0 || understaffed;
    }
    arrays
}

/// Map each RAID array under `disk/md/<array>/...`: a `state` Text plus
/// `degraded`/`failed_disks` gauges and (when the array has redundancy info)
/// `total_disks`/`active_disks` gauges. Every metric carries an `array` label.
pub fn map_mdstat(arrays: &[MdArray]) -> Vec<Metric> {
    let mut out = Vec::new();
    for a in arrays {
        let key = sanitize_key(&a.name);
        let label = |m: Metric| m.label("array", a.name.clone());
        out.push(label(Metric {
            metric: format!("disk/md/{key}/state"),
            value: TelemetryValue::Text(if a.active { "active" } else { "inactive" }.to_string()),
            labels: Vec::new(),
        }));
        out.push(label(Metric::gauge(
            format!("disk/md/{key}/degraded"),
            if a.degraded { 1.0 } else { 0.0 },
        )));
        out.push(label(Metric::gauge(
            format!("disk/md/{key}/failed_disks"),
            a.failed_disks as f64,
        )));
        if let Some(t) = a.total_disks {
            out.push(label(Metric::gauge(
                format!("disk/md/{key}/total_disks"),
                t as f64,
            )));
        }
        if let Some(ac) = a.active_disks {
            out.push(label(Metric::gauge(
                format!("disk/md/{key}/active_disks"),
                ac as f64,
            )));
        }
    }
    out
}

// =============================================================================
// eBPF saturation histograms (#99) — pure histogram → percentile math.
//
// These types and functions are platform-agnostic and feature-independent so
// they unit-test on stable with no kernel: the eBPF poller (behind the `ebpf`
// feature) reads per-CPU BPF arrays, computes a windowed delta, and feeds the
// counts here to build the `LatencyReport` that `@/query/latency` replies with.
// =============================================================================

use serde::{Deserialize, Serialize};
use zensight_sensor_sysinfo_ebpf_common::MAX_SLOTS;

/// One log2 histogram bucket: count of samples with latency `< le_us` µs that
/// did not fall in a lower bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistBucket {
    /// Upper bound of this bucket, in microseconds.
    pub le_us: u64,
    /// Number of samples in this bucket over the window.
    pub count: u64,
}

/// A latency histogram with derived percentiles (all µs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Histogram {
    pub unit: String,
    pub buckets: Vec<HistBucket>,
    pub total: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

/// The `@/query/latency` reply: both saturation histograms over the last window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LatencyReport {
    /// False when the eBPF collector could not load (no caps / unsupported
    /// kernel / not built with `--features ebpf`). The histograms are empty.
    pub available: bool,
    /// Window the bucket counts cover, in seconds.
    pub window_secs: u64,
    /// Scheduler run-queue latency (runqlat).
    pub runqlat: Histogram,
    /// Block-I/O latency (biolatency).
    pub biolatency: Histogram,
}

/// Upper bound (µs) of log2 bucket `i`: bucket 0 → 1, bucket `i` → `2^i`.
pub fn bucket_upper_us(i: usize) -> u64 {
    if i == 0 { 1 } else { 1u64 << i }
}

/// Per-bucket delta of two cumulative snapshots. Saturating, so a counter reset
/// or wrap (cur < prev) clamps that bucket to 0 rather than underflowing.
pub fn windowed_delta(cur: &[u64; MAX_SLOTS], prev: &[u64; MAX_SLOTS]) -> [u64; MAX_SLOTS] {
    let mut out = [0u64; MAX_SLOTS];
    for i in 0..MAX_SLOTS {
        out[i] = cur[i].saturating_sub(prev[i]);
    }
    out
}

/// Approximate percentile as the upper bound (µs) of the bucket the q-th sample
/// falls in (nearest-rank over the log2 counts). `q` in `[0.0, 1.0]`. Returns 0
/// for an empty histogram.
pub fn percentile_us(counts: &[u64; MAX_SLOTS], q: f64) -> u64 {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return 0;
    }
    // Rank of the target sample (1-based), clamped into [1, total].
    let rank = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).clamp(1, total);
    let mut cum = 0u64;
    for (i, &c) in counts.iter().enumerate() {
        cum += c;
        if cum >= rank {
            return bucket_upper_us(i);
        }
    }
    // Unreachable (cum reaches total ≥ rank), but be safe.
    bucket_upper_us(MAX_SLOTS - 1)
}

/// Build a `Histogram` (non-empty buckets + total + p50/p95/p99 + max) from
/// windowed log2 counts.
pub fn build_histogram(counts: &[u64; MAX_SLOTS], unit: &str) -> Histogram {
    let total: u64 = counts.iter().sum();
    let buckets = counts
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c > 0)
        .map(|(i, &c)| HistBucket {
            le_us: bucket_upper_us(i),
            count: c,
        })
        .collect();
    let max_us = counts
        .iter()
        .enumerate()
        .rev()
        .find(|&(_, &c)| c > 0)
        .map(|(i, _)| bucket_upper_us(i))
        .unwrap_or(0);
    Histogram {
        unit: unit.to_string(),
        buckets,
        total,
        p50_us: percentile_us(counts, 0.50),
        p95_us: percentile_us(counts, 0.95),
        p99_us: percentile_us(counts, 0.99),
        max_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_key() {
        // The root mount reduces to the literal `root` (empty chunks are
        // forbidden in Zenoh key expressions).
        assert_eq!(sanitize_key("/"), "root");
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
        let m = map_fd(&FdStat { used: 50, max: 200 });
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

    // --- E. cgroup-v2 -----------------------------------------------------

    #[test]
    fn test_parse_flat_kv() {
        let m = parse_flat_kv("nr_periods 100\nnr_throttled 5\nthrottled_usec 123456\n");
        assert_eq!(m.get("nr_throttled"), Some(&5));
        assert_eq!(m.get("throttled_usec"), Some(&123456));
        assert_eq!(m.get("nr_periods"), Some(&100));
    }

    #[test]
    fn test_parse_cgroup_scalar() {
        assert_eq!(parse_cgroup_scalar("1048576\n"), Some(1048576));
        assert_eq!(parse_cgroup_scalar("max\n"), None);
        assert_eq!(parse_cgroup_scalar("garbage"), None);
    }

    #[test]
    fn test_parse_pressure_file() {
        let fixture = "some avg10=1.50 avg60=0.50 avg300=0.10 total=12345\n\
                       full avg10=0.20 avg60=0.10 avg300=0.00 total=99\n";
        let (some, full) = parse_pressure_file(fixture);
        let some = some.unwrap();
        assert_eq!(some.avg10, 1.50);
        assert_eq!(some.total_us, 12345);
        let full = full.unwrap();
        assert_eq!(full.total_us, 99);
        // cpu.pressure has no `full` line.
        let (s, f) = parse_pressure_file("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
        assert!(s.is_some());
        assert!(f.is_none());
    }

    #[test]
    fn test_map_cgroup_throttle_oom_and_pct() {
        let c = CgroupSample {
            path: "/system.slice/app.service".to_string(),
            cpu_nr_throttled: Some(5),
            cpu_throttled_usec: Some(123456),
            memory_current: Some(50),
            memory_max: Some(200),
            memory_oom_kills: Some(2),
            memory_pressure_full: Some(PressureSample {
                avg10: 9.0,
                total_us: 7,
                ..Default::default()
            }),
            ..Default::default()
        };
        let m = map_cgroup(&c);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "cgroup/cpu/nr_throttled")
                .unwrap()
                .value,
            TelemetryValue::Counter(5)
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "cgroup/memory/oom_kills_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(2)
        );
        // used_percent = 50/200 = 25%.
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "cgroup/memory/used_percent")
                .unwrap()
                .value,
            TelemetryValue::Gauge(25.0)
        );
        assert!(
            m.iter()
                .any(|x| x.metric == "cgroup/memory/pressure/full_total_us")
        );
        // cgroup label is attached.
        assert!(m.iter().all(|x| {
            x.labels
                .iter()
                .any(|(k, v)| *k == "cgroup" && v == "/system.slice/app.service")
        }));
    }

    #[test]
    fn test_map_cgroup_unlimited_memory_no_pct() {
        // memory.max == "max" => memory_max None => no used_percent emitted.
        let c = CgroupSample {
            path: "/".to_string(),
            memory_current: Some(100),
            memory_max: None,
            ..Default::default()
        };
        let m = map_cgroup(&c);
        assert!(!m.iter().any(|x| x.metric == "cgroup/memory/used_percent"));
        assert!(m.iter().any(|x| x.metric == "cgroup/memory/current"));
    }

    // --- G. thermal / power ----------------------------------------------

    #[test]
    fn test_rapl_watts_basic() {
        // 1,000,000 uj over 1s = 1 J/s = 1 W.
        assert_eq!(rapl_watts(0, 1_000_000, None, 1.0), Some(1.0));
        // 2,000,000 uj over 2s = 1 W.
        assert_eq!(rapl_watts(0, 2_000_000, None, 2.0), Some(1.0));
    }

    #[test]
    fn test_rapl_watts_wraparound() {
        // prev near max, cur small: (max-prev)+cur uj.
        // (1000-900)+100 = 200 uj over 1s = 0.0002 W.
        let w = rapl_watts(900, 100, Some(1000), 1.0).unwrap();
        assert!((w - 0.0002).abs() < 1e-9);
        // No max known on wrap => None.
        assert_eq!(rapl_watts(900, 100, None, 1.0), None);
    }

    #[test]
    fn test_rapl_watts_guards() {
        assert_eq!(rapl_watts(0, 100, None, 0.0), None);
        assert_eq!(rapl_watts(0, 100, None, -1.0), None);
    }

    #[test]
    fn test_map_power() {
        let s = PowerSample {
            rapl_watts: vec![("package-0".to_string(), "package-0".to_string(), 12.5)],
            fans: vec![FanReading {
                chip: "nct6798".to_string(),
                label: "fan1".to_string(),
                rpm: 1200.0,
            }],
            batteries: vec![BatteryReading {
                name: "BAT0".to_string(),
                capacity: Some(87.0),
                status: Some("Discharging".to_string()),
            }],
            entropy_avail: Some(3500),
        };
        let m = map_power(&s);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "power/rapl/package-0/watts")
                .unwrap()
                .value,
            TelemetryValue::Gauge(12.5)
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "sensors/nct6798/fan1/rpm")
                .unwrap()
                .value,
            TelemetryValue::Gauge(1200.0)
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "battery/BAT0/capacity")
                .unwrap()
                .value,
            TelemetryValue::Gauge(87.0)
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "battery/BAT0/status")
                .unwrap()
                .value,
            TelemetryValue::Text("Discharging".to_string())
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "system/entropy_avail")
                .unwrap()
                .value,
            TelemetryValue::Gauge(3500.0)
        );
    }

    // --- F. process selector ---------------------------------------------

    #[test]
    fn test_process_selector_defaults() {
        let s = ProcessSelector::parse("");
        assert_eq!(s.sort, ProcessSort::Cpu);
        assert_eq!(s.top, 20);
    }

    #[test]
    fn test_process_selector_parse() {
        let s = ProcessSelector::parse("sort=mem&top=5");
        assert_eq!(s.sort, ProcessSort::Mem);
        assert_eq!(s.top, 5);
        assert_eq!(ProcessSelector::parse("sort=io").sort, ProcessSort::Io);
        assert_eq!(ProcessSelector::parse("sort=memory").sort, ProcessSort::Mem);
        // Unknown sort falls back to cpu.
        assert_eq!(ProcessSelector::parse("sort=bogus").sort, ProcessSort::Cpu);
    }

    #[test]
    fn test_process_selector_clamps_top() {
        assert_eq!(ProcessSelector::parse("top=0").top, 1);
        assert_eq!(
            ProcessSelector::parse("top=99999").top,
            ProcessSelector::MAX_TOP
        );
        // Bad value keeps the default.
        assert_eq!(ProcessSelector::parse("top=abc").top, 20);
    }

    #[test]
    fn test_disk_util_percent() {
        // 500 ms busy over a 1000 ms interval → 50 %.
        assert_eq!(disk_util_percent(500, 1000.0), 50.0);
        // Fully saturated.
        assert_eq!(disk_util_percent(1000, 1000.0), 100.0);
        // Idle device.
        assert_eq!(disk_util_percent(0, 1000.0), 0.0);
    }

    #[test]
    fn test_disk_util_percent_clamped() {
        // io_time delta can exceed the interval (multiqueue / rounding); clamp.
        assert_eq!(disk_util_percent(1500, 1000.0), 100.0);
        // Non-positive interval (first tick / clock skew) → 0, never a divide.
        assert_eq!(disk_util_percent(500, 0.0), 0.0);
        assert_eq!(disk_util_percent(500, -10.0), 0.0);
    }

    #[test]
    fn test_disk_queue_depth() {
        // weighted 2000 ms over 1000 ms busy → average depth 2.0.
        assert_eq!(disk_queue_depth(2000, 1000), 2.0);
        // Sub-unit queue.
        assert_eq!(disk_queue_depth(500, 1000), 0.5);
    }

    #[test]
    fn test_disk_queue_depth_guards_div0() {
        // Idle device: io_time delta 0 → 0, never NaN/inf.
        assert_eq!(disk_queue_depth(0, 0), 0.0);
        assert_eq!(disk_queue_depth(1234, 0), 0.0);
        assert!(disk_queue_depth(1234, 0).is_finite());
    }

    #[test]
    fn test_disk_saturation_combines_both() {
        let s = disk_saturation(500, 1000, 1000.0);
        assert_eq!(s.util_percent, 50.0);
        assert_eq!(s.queue_depth, 2.0);
    }

    #[test]
    fn test_mem_usage_percent_excludes_cache() {
        // 16 GiB total, 12 GiB available → 25 % real pressure, regardless of
        // how much of the in-use 4 GiB is reclaimable cache.
        let total = 16 * 1024 * 1024 * 1024;
        let available = 12 * 1024 * 1024 * 1024;
        assert_eq!(mem_usage_percent(total, available), 25.0);
    }

    #[test]
    fn test_mem_usage_percent_edges() {
        // No memory → 0, never a divide by zero.
        assert_eq!(mem_usage_percent(0, 0), 0.0);
        // available > total (transient kernel estimate) clamps to 0.
        assert_eq!(mem_usage_percent(1000, 2000), 0.0);
        // Fully used.
        assert_eq!(mem_usage_percent(1000, 0), 100.0);
    }

    // --- H. USE-completeness collectors (#98) ----------------------------

    #[test]
    fn test_parse_proc_net_stats_pairs() {
        // Real-shape /proc/net/snmp Tcp header+value (note MaxConn = -1).
        let snmp = "Tcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens \
                    AttemptFails EstabResets CurrEstab InSegs OutSegs RetransSegs InErrs \
                    OutRsts InCsumErrors\n\
                    Tcp: 1 200 120000 -1 2363 42 426 144 10 535447 517811 9905 107 2369 0\n";
        let m = parse_proc_net_stats(snmp);
        assert_eq!(m.get("Tcp:RetransSegs"), Some(&9905));
        assert_eq!(m.get("Tcp:ActiveOpens"), Some(&2363));
        // The signed -1 MaxConn column does not parse as u64 → dropped.
        assert_eq!(m.get("Tcp:MaxConn"), None);
    }

    #[test]
    fn test_parse_netstat_fixture() {
        let snmp = "Tcp: RtoAlgorithm ActiveOpens RetransSegs\n\
                    Tcp: 1 2363 9905\n";
        let netstat = "TcpExt: PruneCalled ListenOverflows ListenDrops TCPHPHits\n\
                       TcpExt: 37 4 7 15114\n";
        let s = parse_netstat(snmp, netstat);
        assert_eq!(s.tcp_retrans_segs, Some(9905));
        assert_eq!(s.listen_overflows, Some(4));
        assert_eq!(s.listen_drops, Some(7));
        let m = map_netstat(&s);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "network/tcp/retrans_segs_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(9905)
        );
        assert!(
            m.iter()
                .any(|x| x.metric == "network/tcp/listen_overflows_total")
        );
        assert!(
            m.iter()
                .any(|x| x.metric == "network/tcp/listen_drops_total")
        );
    }

    #[test]
    fn test_parse_netstat_missing_degrades() {
        // Kernel exposing neither RetransSegs nor the TcpExt block.
        let s = parse_netstat("", "");
        assert_eq!(s, NetstatSample::default());
        assert!(map_netstat(&s).is_empty());
    }

    #[test]
    fn test_parse_sockstat_fixture() {
        let fixture = "sockets: used 1107\n\
                       TCP: inuse 13 orphan 0 tw 2 alloc 17 mem 750\n\
                       UDP: inuse 9 mem 381\n\
                       UDPLITE: inuse 0\n\
                       RAW: inuse 0\n\
                       FRAG: inuse 0 memory 0\n";
        let s = parse_sockstat(fixture);
        assert_eq!(s.sockets_used, Some(1107));
        assert_eq!(s.tcp_inuse, Some(13));
        assert_eq!(s.tcp_mem_pages, Some(750));
        assert_eq!(s.udp_inuse, Some(9));
        let m = map_sockstat(&s);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "network/sockets/tcp_inuse")
                .unwrap()
                .value,
            TelemetryValue::Gauge(13.0)
        );
        assert!(
            m.iter()
                .any(|x| x.metric == "network/sockets/tcp_mem_pages")
        );
        assert!(m.iter().any(|x| x.metric == "network/sockets/udp_inuse"));
        assert!(m.iter().any(|x| x.metric == "network/sockets/used"));
    }

    #[test]
    fn test_parse_softnet_sums_hex_columns() {
        // Two CPUs. Columns are hex: col0 processed, col1 dropped, col2 squeezed.
        // 0x0002a923 = 174371, 0x0001598c = 88460; col2 0x1 + 0x2 = 3.
        let fixture = "0002a923 00000000 00000001 00000000 00000000\n\
                       0001598c 00000005 00000002 00000000 00000000\n";
        let s = parse_softnet(fixture).unwrap();
        assert_eq!(s.processed, 174371 + 88460);
        assert_eq!(s.dropped, 5);
        assert_eq!(s.squeezed, 3);
        let m = map_softnet(&s);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "network/softnet/squeezed_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(3)
        );
        assert!(
            m.iter()
                .any(|x| x.metric == "network/softnet/processed_total")
        );
        assert!(
            m.iter()
                .any(|x| x.metric == "network/softnet/dropped_total")
        );
    }

    #[test]
    fn test_parse_softnet_empty_none() {
        assert!(parse_softnet("").is_none());
        // Short rows (< 3 columns) are ignored.
        assert!(parse_softnet("01 02\n").is_none());
    }

    #[test]
    fn test_parse_schedstat_run_delay() {
        // Real-shape schedstat v17: version/timestamp header, then cpu/domain
        // lines. run_delay is the 8th statistic (index 7) on each cpu<N> line.
        let fixture = "version 17\n\
                       timestamp 4304439726\n\
                       cpu0 0 0 0 0 0 0 2069154225814 687605150808 8600376\n\
                       domain0 SMT 11 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n\
                       cpu1 0 0 0 0 0 0 2194202497845 560152514053 7754232\n\
                       domain0 SMT 22 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
        let s = parse_schedstat(fixture).unwrap();
        assert_eq!(s.per_cpu.len(), 2);
        assert_eq!(s.per_cpu[0], (0, 687605150808));
        assert_eq!(s.per_cpu[1], (1, 560152514053));
        assert_eq!(s.total_run_delay_ns, 687605150808 + 560152514053);
        let m = map_schedstat(&s);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "cpu/schedstat/run_delay_ns_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(687605150808 + 560152514053)
        );
        let c0 = m
            .iter()
            .find(|x| x.metric == "cpu0/schedstat/run_delay_ns_total")
            .unwrap();
        assert_eq!(c0.value, TelemetryValue::Counter(687605150808));
        assert!(c0.labels.contains(&("core", "0".to_string())));
    }

    #[test]
    fn test_parse_schedstat_no_cpu_lines_none() {
        assert!(parse_schedstat("version 17\ntimestamp 123\n").is_none());
    }

    #[test]
    fn test_parse_conntrack_and_map() {
        let s = parse_conntrack("48\n", Some("262144\n")).unwrap();
        assert_eq!(s.count, 48);
        assert_eq!(s.max, Some(262144));
        let m = map_conntrack(&s);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "network/conntrack/count")
                .unwrap()
                .value,
            TelemetryValue::Gauge(48.0)
        );
        let util = m
            .iter()
            .find(|x| x.metric == "network/conntrack/utilization_percent")
            .unwrap();
        if let TelemetryValue::Gauge(p) = util.value {
            assert!((p - (48.0 / 262144.0 * 100.0)).abs() < 1e-9);
        } else {
            panic!("expected gauge");
        }
    }

    #[test]
    fn test_parse_conntrack_missing_max() {
        // Max unreadable (e.g. no CAP_NET_ADMIN): count only, no util%.
        let s = parse_conntrack("48", None).unwrap();
        assert_eq!(s.max, None);
        let m = map_conntrack(&s);
        assert!(m.iter().any(|x| x.metric == "network/conntrack/count"));
        assert!(!m.iter().any(|x| x.metric == "network/conntrack/max"));
        assert!(
            !m.iter()
                .any(|x| x.metric == "network/conntrack/utilization_percent")
        );
        // Bad count => None entirely.
        assert!(parse_conntrack("garbage", Some("1")).is_none());
    }

    #[test]
    fn test_map_edac() {
        let samples = vec![
            EdacSample {
                controller: "mc0".to_string(),
                ce: 12,
                ue: 0,
            },
            EdacSample {
                controller: "mc1".to_string(),
                ce: 0,
                ue: 3,
            },
        ];
        let m = map_edac(&samples);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "memory/edac/mc0/correctable_total")
                .unwrap()
                .value,
            TelemetryValue::Counter(12)
        );
        let ue = m
            .iter()
            .find(|x| x.metric == "memory/edac/mc1/uncorrectable_total")
            .unwrap();
        assert_eq!(ue.value, TelemetryValue::Counter(3));
        assert!(ue.labels.contains(&("controller", "mc1".to_string())));
        // Empty input emits nothing (graceful: no ECC hardware).
        assert!(map_edac(&[]).is_empty());
    }

    #[test]
    fn test_parse_mdstat_clean_array() {
        let fixture = "Personalities : [raid1]\n\
                       md0 : active raid1 sdb1[1] sda1[0]\n\
                       \x20     976630336 blocks super 1.2 [2/2] [UU]\n\
                       \n\
                       unused devices: <none>\n";
        let arrays = parse_mdstat(fixture);
        assert_eq!(arrays.len(), 1);
        let a = &arrays[0];
        assert_eq!(a.name, "md0");
        assert!(a.active);
        assert_eq!(a.total_disks, Some(2));
        assert_eq!(a.active_disks, Some(2));
        assert_eq!(a.failed_disks, 0);
        assert!(!a.degraded);
    }

    #[test]
    fn test_parse_mdstat_degraded_and_failed() {
        // Degraded raid1: one member failed, working < configured.
        let fixture = "Personalities : [raid1]\n\
                       md0 : active raid1 sdb1[1] sda1[0](F)\n\
                       \x20     976630336 blocks super 1.2 [2/1] [_U]\n\
                       \n";
        let arrays = parse_mdstat(fixture);
        assert_eq!(arrays.len(), 1);
        let a = &arrays[0];
        assert_eq!(a.total_disks, Some(2));
        assert_eq!(a.active_disks, Some(1));
        assert_eq!(a.failed_disks, 1);
        assert!(a.degraded);
        let m = map_mdstat(&arrays);
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "disk/md/md0/degraded")
                .unwrap()
                .value,
            TelemetryValue::Gauge(1.0)
        );
        assert_eq!(
            m.iter()
                .find(|x| x.metric == "disk/md/md0/state")
                .unwrap()
                .value,
            TelemetryValue::Text("active".to_string())
        );
        assert!(m.iter().any(|x| x.metric == "disk/md/md0/failed_disks"));
        assert!(
            m.iter()
                .find(|x| x.metric == "disk/md/md0/total_disks")
                .unwrap()
                .labels
                .contains(&("array", "md0".to_string()))
        );
    }

    #[test]
    fn test_parse_mdstat_raid0_no_ratio() {
        // raid0 has no redundancy → no [N/M] token; disks stay None, not degraded.
        let fixture = "md0 : active raid0 sda1[0] sdb1[1]\n\
                       \x20     196608 blocks super 1.2 512k chunks\n";
        let arrays = parse_mdstat(fixture);
        assert_eq!(arrays.len(), 1);
        let a = &arrays[0];
        assert!(a.active);
        assert_eq!(a.total_disks, None);
        assert_eq!(a.active_disks, None);
        assert!(!a.degraded);
        let m = map_mdstat(&arrays);
        // No total/active gauges when the ratio is absent.
        assert!(!m.iter().any(|x| x.metric == "disk/md/md0/total_disks"));
        assert!(m.iter().any(|x| x.metric == "disk/md/md0/state"));
    }

    #[test]
    fn test_parse_mdstat_inactive_is_degraded() {
        let fixture = "md0 : inactive sda1[0](S)\n\
                       \x20     976630336 blocks super 1.2\n";
        let arrays = parse_mdstat(fixture);
        let a = &arrays[0];
        assert!(!a.active);
        assert!(a.degraded);
    }

    #[test]
    fn test_parse_mdstat_empty() {
        let fixture = "Personalities : \nunused devices: <none>\n";
        assert!(parse_mdstat(fixture).is_empty());
    }

    // ---- eBPF latency histogram math (#99) --------------------------------

    #[test]
    fn test_bucket_upper_us() {
        assert_eq!(bucket_upper_us(0), 1);
        assert_eq!(bucket_upper_us(1), 2);
        assert_eq!(bucket_upper_us(10), 1024);
        assert_eq!(bucket_upper_us(MAX_SLOTS - 1), 1u64 << (MAX_SLOTS - 1));
    }

    #[test]
    fn test_windowed_delta_normal_and_reset() {
        let mut cur = [0u64; MAX_SLOTS];
        let mut prev = [0u64; MAX_SLOTS];
        cur[3] = 10;
        prev[3] = 4;
        cur[5] = 2;
        prev[5] = 7; // counter reset/wrap → clamps to 0
        let d = windowed_delta(&cur, &prev);
        assert_eq!(d[3], 6);
        assert_eq!(d[5], 0);
        assert_eq!(d[0], 0);
    }

    #[test]
    fn test_percentile_empty_is_zero() {
        let counts = [0u64; MAX_SLOTS];
        assert_eq!(percentile_us(&counts, 0.5), 0);
        assert_eq!(percentile_us(&counts, 0.99), 0);
    }

    #[test]
    fn test_percentile_single_bucket() {
        let mut counts = [0u64; MAX_SLOTS];
        counts[7] = 100;
        // All mass in one bucket → every percentile is that bucket's bound.
        assert_eq!(percentile_us(&counts, 0.0), bucket_upper_us(7));
        assert_eq!(percentile_us(&counts, 0.5), bucket_upper_us(7));
        assert_eq!(percentile_us(&counts, 1.0), bucket_upper_us(7));
    }

    #[test]
    fn test_percentile_bimodal() {
        let mut counts = [0u64; MAX_SLOTS];
        counts[2] = 90; // fast
        counts[20] = 10; // slow tail
        // p50 lands in the fast bucket, p95/p99 in the slow tail.
        assert_eq!(percentile_us(&counts, 0.50), bucket_upper_us(2));
        assert_eq!(percentile_us(&counts, 0.95), bucket_upper_us(20));
        assert_eq!(percentile_us(&counts, 0.99), bucket_upper_us(20));
    }

    #[test]
    fn test_build_histogram_shape() {
        let mut counts = [0u64; MAX_SLOTS];
        counts[2] = 3;
        counts[9] = 1;
        let h = build_histogram(&counts, "microseconds");
        assert_eq!(h.unit, "microseconds");
        assert_eq!(h.total, 4);
        assert_eq!(h.max_us, bucket_upper_us(9));
        // Only non-empty buckets are emitted, in ascending order.
        assert_eq!(h.buckets.len(), 2);
        assert_eq!(h.buckets[0].le_us, bucket_upper_us(2));
        assert_eq!(h.buckets[0].count, 3);
        assert_eq!(h.buckets[1].le_us, bucket_upper_us(9));
    }

    #[test]
    fn test_latency_report_json_roundtrip() {
        let mut counts = [0u64; MAX_SLOTS];
        counts[4] = 5;
        let report = LatencyReport {
            available: true,
            window_secs: 5,
            runqlat: build_histogram(&counts, "microseconds"),
            biolatency: Histogram::default(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: LatencyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
        // Default report serializes as unavailable with empty histograms.
        let def = LatencyReport::default();
        assert!(!def.available);
        assert_eq!(def.runqlat.total, 0);
    }
}
