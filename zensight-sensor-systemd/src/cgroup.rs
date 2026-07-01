//! cgroup-v2 tree walk (#280): a `systemd-cgls`-style slice→service→scope
//! hierarchy with per-node resource rollups, served on demand from
//! `@/query/cgroups`.
//!
//! Unprivileged: reads the unified hierarchy under `/sys/fs/cgroup` (control files
//! are world-readable). The walk is a point-in-time snapshot keyed by path
//! (transient scopes churn). Depth/breadth are capped. Pure parsers are split out
//! so the whole tree can be built against a fixture directory in tests.

use std::path::{Path, PathBuf};

use zensight_common::query_detail::{CgroupNode, CgroupPid};

/// Default unified cgroup-v2 mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Default `/proc` for pid→comm resolution.
pub const PROC_ROOT: &str = "/proc";

/// Walk bounds (defensive — the tree is served on demand).
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_depth: u32,
    pub max_children: usize,
    pub max_pids: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_depth: 6,
            max_children: 64,
            max_pids: 32,
        }
    }
}

// ─── Pure parsers ────────────────────────────────────────────────────────────

/// Classify a cgroup node from its directory name.
pub fn classify(name: &str) -> &'static str {
    if name.ends_with(".slice") {
        "slice"
    } else if name.ends_with(".service") {
        "service"
    } else if name.ends_with(".scope") {
        "scope"
    } else if name.ends_with(".mount")
        || name.ends_with(".socket")
        || name.ends_with(".timer")
        || name.ends_with(".target")
    {
        "unit"
    } else {
        "other"
    }
}

/// The systemd unit a node maps to (its name, when it carries a unit suffix).
pub fn node_unit(name: &str) -> Option<String> {
    const SUFFIXES: [&str; 7] = [
        ".slice", ".service", ".scope", ".mount", ".socket", ".timer", ".target",
    ];
    SUFFIXES
        .iter()
        .any(|s| name.ends_with(s))
        .then(|| name.to_string())
}

/// Parse a single-value control file (`memory.current`, `pids.current`): a bare
/// integer, or `max` → `None`.
pub fn parse_single(s: &str) -> Option<u64> {
    let t = s.trim();
    if t == "max" || t.is_empty() {
        None
    } else {
        t.parse().ok()
    }
}

/// Parse `cpu.stat` for `usage_usec`.
pub fn parse_cpu_usage_usec(s: &str) -> Option<u64> {
    s.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == "usage_usec").then(|| v.trim().parse().ok())?
    })
}

/// Parse `io.stat`, summing `rbytes`/`wbytes` across all backing devices.
pub fn parse_io_stat(s: &str) -> (u64, u64) {
    let (mut r, mut w) = (0u64, 0u64);
    for line in s.lines() {
        for field in line.split_whitespace() {
            if let Some(v) = field.strip_prefix("rbytes=") {
                r = r.saturating_add(v.parse().unwrap_or(0));
            } else if let Some(v) = field.strip_prefix("wbytes=") {
                w = w.saturating_add(v.parse().unwrap_or(0));
            }
        }
    }
    (r, w)
}

/// Parse `cgroup.procs`: one pid per line.
pub fn parse_procs(s: &str) -> Vec<u32> {
    s.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// Reject path parameters that could escape the cgroup root.
pub fn sanitize_rel(rel: &str) -> Option<String> {
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return Some(String::new());
    }
    if rel
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    Some(rel.to_string())
}

// ─── Tree walk ───────────────────────────────────────────────────────────────

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Resolve a pid to its `comm` under `proc_root` (best-effort).
fn pid_comm(proc_root: &Path, pid: u32) -> String {
    read_trim(&proc_root.join(pid.to_string()).join("comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Build the cgroup subtree rooted at `rel` (relative to `cg_root`). Returns
/// `None` if the directory doesn't exist. Depth/breadth capped by `caps`.
pub fn build_tree(cg_root: &Path, proc_root: &Path, rel: &str, caps: &Caps) -> Option<CgroupNode> {
    let abs = if rel.is_empty() {
        cg_root.to_path_buf()
    } else {
        cg_root.join(rel)
    };
    if !abs.is_dir() {
        return None;
    }
    Some(walk(proc_root, &abs, rel, 0, caps))
}

fn walk(proc_root: &Path, abs: &Path, rel: &str, depth: u32, caps: &Caps) -> CgroupNode {
    let name = abs
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let mem_bytes = read_trim(&abs.join("memory.current")).and_then(|s| parse_single(&s));
    let tasks = read_trim(&abs.join("pids.current")).and_then(|s| parse_single(&s));
    let cpu_usec = read_trim(&abs.join("cpu.stat")).and_then(|s| parse_cpu_usage_usec(&s));
    let (io_r, io_w) = read_trim(&abs.join("io.stat"))
        .map(|s| parse_io_stat(&s))
        .unwrap_or((0, 0));

    // Direct-member processes (leaves, per cgroup-v2 no-internal-process rule).
    let pids: Vec<CgroupPid> = read_trim(&abs.join("cgroup.procs"))
        .map(|s| parse_procs(&s))
        .unwrap_or_default()
        .into_iter()
        .take(caps.max_pids)
        .map(|pid| CgroupPid {
            pid,
            comm: pid_comm(proc_root, pid),
        })
        .collect();

    // Recurse into child cgroup directories (sorted for determinism).
    let mut children = Vec::new();
    if depth < caps.max_depth {
        let mut child_dirs: Vec<PathBuf> = std::fs::read_dir(abs)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        child_dirs.sort();
        for child in child_dirs.into_iter().take(caps.max_children) {
            let child_name = child.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let child_rel = if rel.is_empty() {
                child_name.to_string()
            } else {
                format!("{rel}/{child_name}")
            };
            children.push(walk(proc_root, &child, &child_rel, depth + 1, caps));
        }
    }

    CgroupNode {
        path: rel.to_string(),
        unit: node_unit(&name),
        kind: classify(&name).to_string(),
        name,
        mem_bytes,
        cpu_usec,
        tasks,
        io_read_bytes: (io_r > 0).then_some(io_r),
        io_write_bytes: (io_w > 0).then_some(io_w),
        pids,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn classify_and_unit() {
        assert_eq!(classify("system.slice"), "slice");
        assert_eq!(classify("sshd.service"), "service");
        assert_eq!(classify("session-2.scope"), "scope");
        assert_eq!(classify("boot.mount"), "unit");
        assert_eq!(classify("init.scope-weird"), "other");
        assert_eq!(node_unit("sshd.service").as_deref(), Some("sshd.service"));
        assert_eq!(node_unit("cgroup.procs"), None);
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_single("4096\n"), Some(4096));
        assert_eq!(parse_single("max\n"), None);
        assert_eq!(
            parse_cpu_usage_usec("usage_usec 12345\nuser_usec 6\n"),
            Some(12345)
        );
        assert_eq!(
            parse_io_stat("8:0 rbytes=100 wbytes=200 rios=1\n259:0 rbytes=50 wbytes=0\n"),
            (150, 200)
        );
        assert_eq!(parse_procs("10\n20\n30\n"), vec![10, 20, 30]);
    }

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(
            sanitize_rel("system.slice").as_deref(),
            Some("system.slice")
        );
        assert_eq!(
            sanitize_rel("/system.slice/").as_deref(),
            Some("system.slice")
        );
        assert_eq!(sanitize_rel(""), Some(String::new()));
        assert_eq!(sanitize_rel("../etc"), None);
        assert_eq!(sanitize_rel("a/../b"), None);
    }

    #[test]
    fn build_tree_from_fixture() {
        // Fabricate a tiny cgroupfs: system.slice/{sshd.service, cron.service}.
        let base = std::env::temp_dir().join(format!("zensight-cg-{}", std::process::id()));
        let sys = base.join("system.slice");
        let sshd = sys.join("sshd.service");
        let cron = sys.join("cron.service");
        for d in [&sshd, &cron] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(sys.join("memory.current"), "8192\n").unwrap();
        fs::write(sshd.join("memory.current"), "4096\n").unwrap();
        fs::write(sshd.join("pids.current"), "3\n").unwrap();
        fs::write(sshd.join("cpu.stat"), "usage_usec 555\n").unwrap();
        fs::write(sshd.join("io.stat"), "8:0 rbytes=10 wbytes=20\n").unwrap();
        fs::write(sshd.join("cgroup.procs"), "42\n").unwrap();
        fs::write(cron.join("memory.current"), "max\n").unwrap();

        // Fixture /proc for comm resolution.
        let proc = base.join("proc");
        fs::create_dir_all(proc.join("42")).unwrap();
        fs::write(proc.join("42").join("comm"), "sshd\n").unwrap();

        let caps = Caps::default();
        let tree = build_tree(&base, &proc, "system.slice", &caps).unwrap();
        assert_eq!(tree.name, "system.slice");
        assert_eq!(tree.kind, "slice");
        assert_eq!(tree.mem_bytes, Some(8192));
        assert_eq!(tree.children.len(), 2);

        // children sorted: cron.service before sshd.service.
        let cron_node = &tree.children[0];
        assert_eq!(cron_node.name, "cron.service");
        assert_eq!(cron_node.mem_bytes, None); // "max"

        let sshd_node = &tree.children[1];
        assert_eq!(sshd_node.name, "sshd.service");
        assert_eq!(sshd_node.kind, "service");
        assert_eq!(sshd_node.unit.as_deref(), Some("sshd.service"));
        assert_eq!(sshd_node.mem_bytes, Some(4096));
        assert_eq!(sshd_node.cpu_usec, Some(555));
        assert_eq!(sshd_node.tasks, Some(3));
        assert_eq!(sshd_node.io_read_bytes, Some(10));
        assert_eq!(sshd_node.io_write_bytes, Some(20));
        assert_eq!(
            sshd_node.pids,
            vec![CgroupPid {
                pid: 42,
                comm: "sshd".into()
            }]
        );
        assert_eq!(sshd_node.path, "system.slice/sshd.service");

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn build_tree_missing_path_is_none() {
        assert!(
            build_tree(
                Path::new("/nonexistent"),
                Path::new("/proc"),
                "x",
                &Caps::default()
            )
            .is_none()
        );
    }

    #[test]
    fn depth_cap_stops_recursion() {
        let base = std::env::temp_dir().join(format!("zensight-cg-depth-{}", std::process::id()));
        let deep = base.join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        let caps = Caps {
            max_depth: 1,
            ..Caps::default()
        };
        let tree = build_tree(&base, Path::new("/proc"), "", &caps).unwrap();
        // depth 0 = root, depth 1 = "a"; "a"'s children NOT walked.
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].name, "a");
        assert!(tree.children[0].children.is_empty());
        fs::remove_dir_all(&base).ok();
    }
}
