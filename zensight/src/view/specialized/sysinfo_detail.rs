//! On-demand sysinfo process-explorer client: fetches the per-process table from
//! the sensor's `@/query/processes` channel (principle P2 — the per-pid firehose
//! is never streamed; the GUI pulls it only when a user drills into a host).
//!
//! Reuses the Iced-independent [`fetch_records`](super::netlink_detail::fetch_records)
//! so the fetch+decode path is shared and already integration-tested.

use std::sync::Arc;

use zensight_common::ProcessRecord;

use crate::view::specialized::fetch::Fetch;

/// How many processes the explorer asks the sensor for.
const TOP_N: usize = 50;

/// How to sort the process table (mirrors the sensor's `ProcessSort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessSort {
    #[default]
    Cpu,
    Mem,
    Io,
}

impl ProcessSort {
    /// The `sort=` selector token the sensor's `ProcessSelector::parse` expects.
    pub fn token(&self) -> &'static str {
        match self {
            ProcessSort::Cpu => "cpu",
            ProcessSort::Mem => "mem",
            ProcessSort::Io => "io",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ProcessSort::Cpu => "CPU",
            ProcessSort::Mem => "Memory",
            ProcessSort::Io => "I/O",
        }
    }
}

/// The process-explorer procedure selector for a sort order. v1: the fleet
/// selector reaches every sysinfo origin; per-host narrowing (targeting the
/// selected host's origin key instead of `*`) is a follow-up — it needs the
/// hostname→origin map the health documents carry. Single-host deployments
/// the single-instance netlink/netring sensors.
pub fn processes_key(_host: &str, sort: ProcessSort) -> String {
    format!(
        "{}?sort={}&top={TOP_N}",
        zensight_common::fleet_rpc_key("sysinfo", "processes"),
        sort.token()
    )
}

/// A pid pivot into the process explorer (#313): carried by
/// [`crate::message::Message::PivotToProcess`], rendered as a filter banner.
/// `start_time` is the `(pid, start_time)` identity pair from the pivot origin
/// (unit MainPID, socket owner) — the stale-generation guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidFilter {
    pub pid: i32,
    pub start_time: Option<u64>,
}

/// The stale-generation verdict for a pid filter over the fetched table (#313).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidVerdict {
    /// The pid is present and (when known) the start_time matches — same process.
    Live,
    /// The pid exists but with a different start_time: the original process
    /// exited and the kernel reused its pid. Never show the impostor as a match.
    Reused,
    /// The pid is not in the fetched table (exited, or below the top-N cut).
    Gone,
}

/// Judge a pid filter against the fetched process table (#313). Pure.
pub fn pid_filter_verdict(procs: &[ProcessRecord], filter: &PidFilter) -> PidVerdict {
    match procs.iter().find(|p| p.pid == filter.pid) {
        Some(p) => match filter.start_time {
            Some(want) if p.start_time != want => PidVerdict::Reused,
            _ => PidVerdict::Live,
        },
        None => PidVerdict::Gone,
    }
}

/// On-demand process detail fetched for the selected sysinfo host.
#[derive(Debug, Clone, Default)]
pub struct SysinfoDetailState {
    pub processes: Fetch<Vec<ProcessRecord>>,
    /// The sort the last fetch used (drives the active toggle highlight).
    pub sort: ProcessSort,
    /// Active pid pivot (#313): filters the explorer to one process, with the
    /// stale-generation guard. `None` = normal explorer.
    pub pid_filter: Option<PidFilter>,
}

impl SysinfoDetailState {
    /// Mark a process fetch as in flight under the given sort.
    pub fn loading(&mut self, sort: ProcessSort) {
        self.sort = sort;
        self.processes = Fetch::Loading;
    }

    /// Store the process fetch outcome.
    pub fn apply(&mut self, result: Result<Vec<ProcessRecord>, String>) {
        self.processes = Fetch::from_result(result);
    }
}

/// Fetch + decode the process table for `host` sorted by `sort`.
pub async fn fetch_processes(
    session: Arc<zenoh::Session>,
    host: String,
    sort: ProcessSort,
) -> Option<Vec<ProcessRecord>> {
    super::netlink_detail::fetch_records(session, processes_key(&host, sort)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_host_scoped_with_sort_and_top() {
        assert_eq!(
            processes_key("server01", ProcessSort::Cpu),
            "zensight/@v1/*/@rpc/sysinfo/processes?sort=cpu&top=50"
        );
        assert_eq!(
            processes_key("server01", ProcessSort::Mem),
            "zensight/@v1/*/@rpc/sysinfo/processes?sort=mem&top=50"
        );
        assert_eq!(ProcessSort::Io.token(), "io");
    }

    fn proc(pid: i32, start_time: u64) -> ProcessRecord {
        ProcessRecord {
            pid,
            name: "p".into(),
            cpu: 0.0,
            rss: 0,
            vsz: 0,
            threads: None,
            state: "S".into(),
            io_read: 0,
            io_write: 0,
            uid: None,
            cmdline: String::new(),
            exe: None,
            ppid: None,
            cgroup: None,
            start_time,
            user: None,
        }
    }

    #[test]
    fn pid_filter_verdict_guards_generations() {
        let procs = vec![proc(42, 1000), proc(43, 2000)];
        // Same pid + same start_time → the same process.
        let f = PidFilter {
            pid: 42,
            start_time: Some(1000),
        };
        assert_eq!(pid_filter_verdict(&procs, &f), PidVerdict::Live);
        // Same pid, different start_time → the kernel reused the pid.
        let f = PidFilter {
            pid: 42,
            start_time: Some(999),
        };
        assert_eq!(pid_filter_verdict(&procs, &f), PidVerdict::Reused);
        // Unknown start_time (origin didn't carry one) → best-effort match.
        let f = PidFilter {
            pid: 42,
            start_time: None,
        };
        assert_eq!(pid_filter_verdict(&procs, &f), PidVerdict::Live);
        // Absent pid → exited (or below the fetch cut).
        let f = PidFilter {
            pid: 99,
            start_time: Some(1),
        };
        assert_eq!(pid_filter_verdict(&procs, &f), PidVerdict::Gone);
    }

    #[test]
    fn apply_stores_processes_and_remembers_sort() {
        let mut s = SysinfoDetailState::default();
        s.loading(ProcessSort::Mem);
        assert!(s.processes.is_loading());
        assert_eq!(s.sort, ProcessSort::Mem);
        s.apply(Ok(vec![ProcessRecord {
            pid: 42,
            name: "redis-server".into(),
            cpu: 12.5,
            rss: 1024,
            vsz: 4096,
            threads: Some(4),
            state: "Run".into(),
            io_read: 0,
            io_write: 0,
            uid: Some(1000),
            cmdline: "redis-server *:6379".into(),
            exe: Some("/usr/bin/redis-server".into()),
            ppid: Some(1),
            cgroup: Some("/system.slice/redis.service".into()),
            start_time: 12345,
            user: Some("redis".into()),
        }]));
        assert_eq!(s.processes.ready().map(|v| v.len()), Some(1));
        s.apply(Err("no sensor".into()));
        assert_eq!(s.processes.error(), Some("no sensor"));
    }
}
