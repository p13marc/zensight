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

/// The process-explorer queryable key for a given host + sort. The sysinfo query
/// channel is host-scoped (`zensight/sysinfo/<host>/@/query/processes`), unlike
/// the single-instance netlink/netring sensors.
pub fn processes_key(host: &str, sort: ProcessSort) -> String {
    format!(
        "zensight/sysinfo/{host}/@/query/processes?sort={}&top={TOP_N}",
        sort.token()
    )
}

/// On-demand process detail fetched for the selected sysinfo host.
#[derive(Debug, Clone, Default)]
pub struct SysinfoDetailState {
    pub processes: Fetch<Vec<ProcessRecord>>,
    /// The sort the last fetch used (drives the active toggle highlight).
    pub sort: ProcessSort,
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
            "zensight/sysinfo/server01/@/query/processes?sort=cpu&top=50"
        );
        assert_eq!(
            processes_key("server01", ProcessSort::Mem),
            "zensight/sysinfo/server01/@/query/processes?sort=mem&top=50"
        );
        assert_eq!(ProcessSort::Io.token(), "io");
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
        }]));
        assert_eq!(s.processes.ready().map(|v| v.len()), Some(1));
        s.apply(Err("no sensor".into()));
        assert_eq!(s.processes.error(), Some("no sensor"));
    }
}
