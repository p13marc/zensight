//! cgroup-v2 resource reads (#819).
//!
//! The runtime knows *what* a container is; the kernel knows what it is
//! *doing*. On 2026-08-17 five sensors shared one cgroup and one `MemoryMax`,
//! so per-container memory did not exist as a number and the OOM was
//! attributed to "the bundle" for eleven days. These are the files that make
//! it exist.
//!
//! Every read is best-effort and every absence is `None`, never zero: a
//! rootless container's cgroup may be unreadable, PSI may be off, and
//! `memory.peak` is newer than some kernels. A zero for any of those reads as
//! "idle", which is the worst available answer.

use std::path::{Path, PathBuf};

use zensight_common::container::{
    ContainerResources, parse_flat_keyed, parse_limit, parse_pressure_avg10,
};

/// The unified-hierarchy mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Resolve a container's cgroup directory.
///
/// The runtime reports an absolute cgroup path (`/machine.slice/libpod-<id>.scope`
/// rootful, `/user.slice/user-1000.slice/…` rootless), which is relative to
/// the unified mount rather than to `/`. Joining it naively with `Path::join`
/// would discard the root, so the leading slash is stripped deliberately.
pub fn resolve(root: &Path, cgroup_path: Option<&str>, id: &str) -> Option<PathBuf> {
    if let Some(p) = cgroup_path.filter(|p| !p.is_empty()) {
        let dir = root.join(p.trim_start_matches('/'));
        if dir.is_dir() {
            return Some(dir);
        }
    }
    // Fallback for runtimes that do not report the path: the conventional
    // rootful location. Tried last so a reported path always wins.
    let guess = root.join(format!("machine.slice/libpod-{id}.scope"));
    guess.is_dir().then_some(guess)
}

fn read(dir: &Path, file: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(file)).ok()
}

/// Read everything the kernel will say about one container.
pub fn read_resources(dir: &Path) -> ContainerResources {
    let cpu = read(dir, "cpu.stat")
        .map(|t| {
            parse_flat_keyed(&t)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();
    let events = read(dir, "memory.events")
        .map(|t| {
            parse_flat_keyed(&t)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();

    ContainerResources {
        memory_bytes: read(dir, "memory.current").and_then(|t| t.trim().parse().ok()),
        // `max` is not a limit of zero — see `parse_limit`.
        memory_max_bytes: read(dir, "memory.max").and_then(|t| parse_limit(&t)),
        memory_peak_bytes: read(dir, "memory.peak").and_then(|t| t.trim().parse().ok()),
        cpu_usage_usec: cpu.get("usage_usec").copied(),
        cpu_throttled_usec: cpu.get("throttled_usec").copied(),
        oom_kills: events.get("oom_kill").copied(),
        memory_max_events: events.get("max").copied(),
        cpu_pressure_avg10: read(dir, "cpu.pressure").and_then(|t| parse_pressure_avg10(&t)),
        memory_pressure_avg10: read(dir, "memory.pressure").and_then(|t| parse_pressure_avg10(&t)),
        io_pressure_avg10: read(dir, "io.pressure").and_then(|t| parse_pressure_avg10(&t)),
        pids: read(dir, "pids.current").and_then(|t| t.trim().parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_full_cgroup_reads_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "memory.current", "134217728\n");
        write(d, "memory.max", "268435456\n");
        write(d, "memory.peak", "200000000\n");
        write(
            d,
            "cpu.stat",
            "usage_usec 900\nuser_usec 700\nthrottled_usec 12\n",
        );
        write(
            d,
            "memory.events",
            "low 0\nhigh 0\nmax 5\noom 1\noom_kill 1\n",
        );
        write(d, "cpu.pressure", "some avg10=2.50 avg60=1.00 total=1\n");
        write(d, "pids.current", "17\n");

        let r = read_resources(d);
        assert_eq!(r.memory_bytes, Some(134217728));
        assert_eq!(r.memory_max_bytes, Some(268435456));
        assert_eq!(r.memory_peak_bytes, Some(200000000));
        assert_eq!(r.cpu_usage_usec, Some(900));
        assert_eq!(r.cpu_throttled_usec, Some(12));
        // The number that names a victim.
        assert_eq!(r.oom_kills, Some(1));
        assert_eq!(r.memory_max_events, Some(5));
        assert_eq!(r.cpu_pressure_avg10, Some(2.50));
        assert_eq!(r.pids, Some(17));
        assert_eq!(r.memory_pressure_avg10, None, "absent stays absent");
    }

    /// Every field is optional and every absence is `None`. A zero here would
    /// read as "idle", which is the worst answer available for a container
    /// whose cgroup the sensor simply cannot see.
    #[test]
    fn an_unreadable_cgroup_reports_nothing_rather_than_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let r = read_resources(tmp.path());
        assert_eq!(r.memory_bytes, None);
        assert_eq!(r.oom_kills, None);
        assert_eq!(r.cpu_usage_usec, None);
    }

    /// An unlimited container reports no ceiling, not a ceiling of zero.
    #[test]
    fn an_unlimited_container_has_no_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "memory.max", "max\n");
        assert_eq!(read_resources(tmp.path()).memory_max_bytes, None);
    }

    /// The runtime's path is absolute against the cgroup mount, not against
    /// `/`. `Path::join` on it would silently discard the root and read the
    /// host's own cgroup files instead of the container's.
    #[test]
    fn an_absolute_runtime_path_resolves_under_the_cgroup_root() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("machine.slice/libpod-abc.scope");
        std::fs::create_dir_all(&nested).unwrap();
        let got = resolve(tmp.path(), Some("/machine.slice/libpod-abc.scope"), "abc");
        assert_eq!(got.as_deref(), Some(nested.as_path()));
    }

    #[test]
    fn a_missing_cgroup_resolves_to_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve(tmp.path(), Some("/nope"), "abc"), None);
        assert_eq!(resolve(tmp.path(), None, "abc"), None);
    }
}
