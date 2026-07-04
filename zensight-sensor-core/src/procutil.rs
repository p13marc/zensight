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
}
