//! Small `/proc/<pid>/*` parsers shared across sensors (#302/#303/#304).
//!
//! Process identity everywhere in ZenSight is the **`(pid, start_time)` pair**
//! (the OTel semconv rule — bare PIDs get reused), where `start_time` is
//! `/proc/<pid>/stat` field 22 in clock ticks since boot. This matches nlink's
//! `ProcessRef.start_time` byte-for-byte, so joins across sysinfo, systemd and
//! netlink records need no unit conversion.

/// Read `/proc/<pid>/stat` field 22 (`starttime`, clock ticks since boot).
/// `None` when the process is gone or unreadable.
pub fn proc_start_time_ticks(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat_starttime(&stat)
}

/// Parse `starttime` out of a stat line. The `comm` field (2) may contain
/// spaces and parentheses, so fields resume after the **last** `)` — from
/// there, `state` is field 3 and `starttime` (field 22) is token index 19.
fn parse_stat_starttime(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Read the cgroup v2 unified path from `/proc/<pid>/cgroup` (the `0::<path>`
/// line). This is **the join key to systemd units**
/// (`process.cgroup == unit.control_group`). `None` when the process is gone,
/// unreadable, or on a cgroup-v1-only host.
pub fn proc_cgroup_v2(pid: i32) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_cgroup_v2(&content)
}

fn parse_cgroup_v2(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
}

// ─── Self-measurement (#811) ─────────────────────────────────────────────────
//
// The sensor reading its own `/proc/self/*` on the health tick. Kept here —
// pure parsers plus thin readers — rather than pulling the sysinfo crate into
// sensor-core: three files, four fields, no dependency.

/// `(rss_bytes, vsz_bytes)` from `/proc/self/status` (`VmRSS`/`VmSize`, kB).
/// `None` when either line is missing (non-Linux, or a hostile mount).
pub fn self_memory() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_self_memory(&status)
}

fn parse_self_memory(status: &str) -> Option<(u64, u64)> {
    let kb = |prefix: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .and_then(|rest| {
                rest.trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
    };
    Some((kb("VmRSS:")? * 1024, kb("VmSize:")? * 1024))
}

/// Self CPU usage between calls: holds the previous `utime+stime` reading and
/// returns the busy fraction (percent of one core) over the elapsed interval.
///
/// Ticks-per-second is hardcoded to `USER_HZ = 100`, which is universal on
/// Linux for the fields `/proc/<pid>/stat` reports — taking a libc dependency
/// for `sysconf(_SC_CLK_TCK)` buys nothing on any host this runs on, and
/// `cpu_percent` is an optional field besides.
#[derive(Debug, Default)]
pub struct SelfCpuSampler {
    prev: Option<(u64, std::time::Instant)>,
}

const USER_HZ: f64 = 100.0;

impl SelfCpuSampler {
    /// The CPU busy percent since the previous call; `None` on the first
    /// call (nothing to diff against) or when `/proc/self/stat` is
    /// unreadable — never a guessed zero.
    pub fn sample(&mut self) -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let ticks = parse_self_cpu_ticks(&stat)?;
        let now = std::time::Instant::now();
        let out = match self.prev {
            Some((prev_ticks, prev_at)) => {
                let dt = now.duration_since(prev_at).as_secs_f64();
                (dt > 0.0).then(|| (ticks.saturating_sub(prev_ticks) as f64 / USER_HZ) / dt * 100.0)
            }
            None => None,
        };
        self.prev = Some((ticks, now));
        out
    }
}

/// `utime + stime` from a stat line — fields 14 and 15, token indices 11/12
/// after the last `)` (the `comm` field may contain spaces and parentheses).
fn parse_self_cpu_ticks(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(')')?.1;
    let mut it = rest.split_whitespace().skip(11);
    let utime: u64 = it.next()?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some(utime + stime)
}

/// The sensor's own cgroup-v2 memory context (#811): resolve its unified
/// path from `/proc/self/cgroup`, then read `memory.{current,max,high}` and
/// the `memory.events` OOM counters. A minimal mirror of the sysinfo
/// sensor's richer cgroup collector — deliberately not a shared module, so
/// sensor-core stays dependency-light. `None` when not under cgroup-v2.
pub fn self_cgroup() -> Option<zensight_common::CgroupSelf> {
    let path = proc_cgroup_v2(std::process::id() as i32)?;
    Some(read_cgroup_memory(std::path::Path::new(&format!(
        "/sys/fs/cgroup{path}"
    ))))
}

fn read_cgroup_memory(dir: &std::path::Path) -> zensight_common::CgroupSelf {
    let num = |file: &str| -> Option<u64> {
        let text = std::fs::read_to_string(dir.join(file)).ok()?;
        parse_cgroup_number(&text)
    };
    let events = std::fs::read_to_string(dir.join("memory.events"))
        .ok()
        .unwrap_or_default();
    zensight_common::CgroupSelf {
        memory_current_bytes: num("memory.current"),
        memory_max_bytes: num("memory.max"),
        memory_high_bytes: num("memory.high"),
        oom_kills: parse_events_field(&events, "oom_kill"),
        oom_events: parse_events_field(&events, "oom"),
    }
}

/// A cgroup numeric file: a number, or the literal `max` — which means
/// *unlimited* and maps to `None` (there is no limit to report).
fn parse_cgroup_number(text: &str) -> Option<u64> {
    let t = text.trim();
    if t == "max" { None } else { t.parse().ok() }
}

/// One `key value` line out of `memory.events` (exact key match: `oom` must
/// not match `oom_kill`).
fn parse_events_field(events: &str, key: &str) -> Option<u64> {
    events.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starttime_survives_hostile_comm() {
        // comm with spaces AND a ')' — the classic stat-parsing trap.
        let stat = "1234 (my (we)ird proc) S 1 1234 1234 0 -1 4194560 500 0 0 0 \
                    10 5 0 0 20 0 4 0 987654 1000000 250 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(parse_stat_starttime(stat), Some(987654));
    }

    #[test]
    fn starttime_none_on_garbage() {
        assert_eq!(parse_stat_starttime("not a stat line"), None);
        assert_eq!(parse_stat_starttime("1 (x) S 1 2 3"), None);
    }

    #[test]
    fn cgroup_v2_line_extracted() {
        let content = "1:name=systemd:/legacy\n0::/system.slice/sshd.service\n";
        assert_eq!(
            parse_cgroup_v2(content).as_deref(),
            Some("/system.slice/sshd.service")
        );
        // v1-only host: no 0:: line.
        assert_eq!(parse_cgroup_v2("2:cpu:/foo\n"), None);
        // Empty path filtered.
        assert_eq!(parse_cgroup_v2("0::\n"), None);
    }

    // ── #811 self-measurement parsers ──

    #[test]
    fn self_memory_parses_vmrss_and_vmsize() {
        let status = "Name:\tzensight\nVmPeak:\t  200000 kB\nVmSize:\t  150000 kB\n\
                      VmRSS:\t   50000 kB\nThreads:\t8\n";
        assert_eq!(
            parse_self_memory(status),
            Some((50_000 * 1024, 150_000 * 1024))
        );
        // Either line missing → None, never a guessed zero.
        assert_eq!(parse_self_memory("Name: x\nVmRSS: 5 kB\n"), None);
        assert_eq!(parse_self_memory(""), None);
    }

    #[test]
    fn self_cpu_ticks_survive_hostile_comm() {
        // utime=10 stime=5 (fields 14/15) behind a comm with ')' and spaces.
        let stat = "1234 (my (we)ird proc) S 1 1234 1234 0 -1 4194560 500 0 0 0 \
                    10 5 0 0 20 0 4 0 987654 1000000 250 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(parse_self_cpu_ticks(stat), Some(15));
        assert_eq!(parse_self_cpu_ticks("garbage"), None);
    }

    #[test]
    fn cgroup_max_literal_means_no_limit() {
        assert_eq!(parse_cgroup_number("max\n"), None);
        assert_eq!(parse_cgroup_number("1073741824\n"), Some(1_073_741_824));
        assert_eq!(parse_cgroup_number("junk"), None);
    }

    #[test]
    fn events_field_is_exact_key_match() {
        let events = "low 0\nhigh 3\nmax 1\noom 2\noom_kill 1\noom_group_kill 0\n";
        // `oom` must not match `oom_kill` (or vice versa).
        assert_eq!(parse_events_field(events, "oom"), Some(2));
        assert_eq!(parse_events_field(events, "oom_kill"), Some(1));
        assert_eq!(parse_events_field(events, "absent"), None);
    }
}
