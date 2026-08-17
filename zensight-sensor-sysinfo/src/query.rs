//! On-demand per-process detail query channel (principle P2, plan §F).
//!
//! Declares `zensight/v1/<origin>/@rpc/sysinfo/processes`. The GUI calls it when a
//! user drills into a host to ask "what's eating the box?". Each reply is a
//! fresh, sorted, bounded `Vec<ProcessRecord>` serialized as JSON — the
//! high-cardinality per-pid firehose is *never* streamed onto the telemetry bus
//! (only the small `system/processes_{total,zombie}` aggregates are streamed).
//!
//! Selector: `?sort=cpu|mem|io&top=N` (parsed by [`ProcessSelector`]).
//!
//! The full `/proc/<pid>/*` walk is blocking I/O, so it runs under
//! `tokio::task::spawn_blocking` per the Plan-05 async contract (bounded
//! per-entity iteration off the runtime thread).

use std::sync::Arc;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use zensight_common::query_detail::ProcessRecord;
use zensight_sensor_core::scrub::{ArgScrubber, CMDLINE_CAP_BYTES};

use crate::config::ProcessScrubConfig;
use crate::map::{ProcessSelector, ProcessSort};

/// How cmdlines leave the host (#302): compiled once from
/// [`ProcessScrubConfig`], shared by every query.
struct CmdlinePolicy {
    /// `None` = arguments stripped entirely.
    scrubber: Option<ArgScrubber>,
    /// Scrubbing disabled (`scrub_args: false`) — raw argv, still capped.
    raw: bool,
}

impl CmdlinePolicy {
    fn new(cfg: &ProcessScrubConfig) -> Self {
        if cfg.strip_proc_arguments {
            CmdlinePolicy {
                scrubber: None,
                raw: false,
            }
        } else if cfg.scrub_args {
            CmdlinePolicy {
                scrubber: Some(ArgScrubber::new(&cfg.custom_sensitive_words)),
                raw: false,
            }
        } else {
            CmdlinePolicy {
                scrubber: None,
                raw: true,
            }
        }
    }

    fn render(&self, argv: &[String]) -> String {
        if let Some(scrubber) = &self.scrubber {
            scrubber.scrub_cmdline(argv, CMDLINE_CAP_BYTES)
        } else if self.raw {
            let mut s = argv.join(" ");
            if s.len() > CMDLINE_CAP_BYTES {
                let mut end = CMDLINE_CAP_BYTES;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s.push('…');
            }
            s
        } else {
            String::new() // strip_proc_arguments
        }
    }
}

/// Run the per-process detail query channel until the session closes.
///
/// `producer` is the sensor's producer name (`sysinfo`); the queryable lives
/// at the v1 procedure key `@rpc/sysinfo/processes`.
pub async fn run(
    session: Arc<zenoh::Session>,
    producer: String,
    _source: String,
    scrub: ProcessScrubConfig,
) {
    let key = zensight_keyspace_ctx(&producer).rpc_key(&["processes"]);
    let queryable = match zensight_common::served::serve_queryable(&session, key.as_keyexpr()).await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare processes failed");
            return;
        }
    };
    tracing::info!(key = %key, "per-process detail query channel ready");

    let policy = Arc::new(CmdlinePolicy::new(&scrub));
    while let Ok(query) = queryable.recv_async().await {
        let sel = ProcessSelector::parse(query.parameters().as_str());
        let policy = policy.clone();
        // The per-pid /proc walk is blocking — keep it off the runtime thread.
        let records =
            match tokio::task::spawn_blocking(move || collect_processes(sel, &policy)).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "query: process walk task failed");
                    Vec::new()
                }
            };
        reply_json(&query, &key, &records).await;
    }
}

/// Serve the opt-in eBPF saturation histograms on `@rpc/sysinfo/latency`
/// (issue #99).
///
/// Reads the shared snapshot the eBPF poller maintains (runqlat + biolatency,
/// never streamed) and replies it as JSON.
///
/// Declared unconditionally — this module is not behind the `ebpf` feature, and
/// `main` spawns it whatever the build — so the GUI can tell "no sensor replied"
/// apart from a sensor replying `available: false` (binary built without the
/// feature, `collect.ebpf` off, or load/attach failed).
pub async fn run_latency(
    session: Arc<zenoh::Session>,
    producer: String,
    _source: String,
    report: Arc<std::sync::Mutex<crate::map::LatencyReport>>,
) {
    let key = zensight_keyspace_ctx(&producer).rpc_key(&["latency"]);
    let queryable = match zensight_common::served::serve_queryable(&session, key.as_keyexpr()).await
    {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare latency failed");
            return;
        }
    };
    tracing::info!(key = %key, "eBPF latency query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let snapshot = report.lock().map(|r| r.clone()).unwrap_or_default();
        reply_json(&query, &key, &snapshot).await;
    }
}

/// Serialize `records` as JSON and reply on the query's own key.
/// Small helper: the v1 context for one producer.
fn zensight_keyspace_ctx(producer: &str) -> zensight_sensor_core::v1::V1Context {
    zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, producer)
}

/// Reply on the queryable's **concrete** key (RFC 05 §2.1), never the
/// query's selector.
async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, key: &str, records: &T) {
    match serde_json::to_vec(records) {
        Ok(payload) => {
            if let Err(e) = query.reply(key, payload).await {
                tracing::warn!(error = %e, "query: reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "query: serialize failed"),
    }
}

/// Build a fresh `System`, snapshot every process, rank by the selector, and
/// return the top-N as wire DTOs. Blocking — call under `spawn_blocking`.
fn collect_processes(sel: ProcessSelector, policy: &CmdlinePolicy) -> Vec<ProcessRecord> {
    let mut sys = System::new();
    // CPU usage needs two samples a short interval apart to be meaningful.
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );

    let mut records: Vec<ProcessRecord> = sys
        .processes()
        .values()
        .map(|p| {
            let io = p.disk_usage();
            ProcessRecord {
                pid: p.pid().as_u32() as i32,
                name: p.name().to_string_lossy().to_string(),
                cpu: p.cpu_usage(),
                rss: p.memory(),
                vsz: p.virtual_memory(),
                threads: p.tasks().map(|t| t.len()),
                state: p.status().to_string(),
                io_read: io.total_read_bytes,
                io_write: io.total_written_bytes,
                uid: p.user_id().and_then(|u| u.to_string().parse::<u32>().ok()),
                cmdline: String::new(),
                exe: None,
                ppid: None,
                cgroup: None,
                start_time: 0,
                user: None,
            }
        })
        .collect();

    match sel.sort {
        ProcessSort::Cpu => records.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProcessSort::Mem => records.sort_by_key(|r| std::cmp::Reverse(r.rss)),
        ProcessSort::Io => records.sort_by_key(|r| std::cmp::Reverse(r.io_read + r.io_write)),
    }
    records.truncate(sel.top);

    // Identity enrichment (#302) AFTER sort+truncate, so the per-pid procfs
    // reads are bounded by `top` (≤ 200), not by the process count.
    let users = sysinfo::Users::new_with_refreshed_list();
    for rec in &mut records {
        if let Some(p) = sys.process(sysinfo::Pid::from_u32(rec.pid as u32)) {
            let argv: Vec<String> = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            rec.cmdline = policy.render(&argv);
            // May be None for other users' processes without ptrace perms —
            // documented degradation, not an error.
            rec.exe = p.exe().map(|e| e.display().to_string());
            rec.ppid = p.parent().map(|pp| pp.as_u32() as i32);
            rec.user = p
                .user_id()
                .and_then(|uid| users.get_user_by_id(uid))
                .map(|u| u.name().to_string());
        }
        rec.start_time =
            zensight_sensor_core::procutil::proc_start_time_ticks(rec.pid).unwrap_or(0);
        rec.cgroup = zensight_sensor_core::procutil::proc_cgroup_v2(rec.pid);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_processes_smoke() {
        // Live walk of the current host: should find at least this test process,
        // honour the top bound, and never panic on any field.
        let sel = ProcessSelector {
            sort: ProcessSort::Mem,
            top: 5,
        };
        let policy = CmdlinePolicy::new(&ProcessScrubConfig::default());
        let recs = collect_processes(sel, &policy);
        assert!(recs.len() <= 5);
        // Sorted descending by rss.
        for w in recs.windows(2) {
            assert!(w[0].rss >= w[1].rss);
        }
        // Enrichment: every record has a start_time (own/visible processes);
        // cmdline stays within the cap.
        for r in &recs {
            assert!(r.cmdline.len() <= CMDLINE_CAP_BYTES + '…'.len_utf8());
        }
        assert!(recs.iter().any(|r| r.start_time > 0));
    }

    #[test]
    fn cmdline_policy_modes() {
        let argv = vec!["app".to_string(), "--password=x".to_string()];
        let scrub = CmdlinePolicy::new(&ProcessScrubConfig::default());
        assert_eq!(scrub.render(&argv), "app --password=********");
        let strip = CmdlinePolicy::new(&ProcessScrubConfig {
            strip_proc_arguments: true,
            ..Default::default()
        });
        assert_eq!(strip.render(&argv), "");
        let raw = CmdlinePolicy::new(&ProcessScrubConfig {
            scrub_args: false,
            ..Default::default()
        });
        assert_eq!(raw.render(&argv), "app --password=x");
    }
}
