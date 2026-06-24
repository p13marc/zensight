//! On-demand per-process detail query channel (principle P2, plan §F).
//!
//! Declares `zensight/sysinfo/<host>/@/query/processes`. The GUI calls it when a
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

use crate::map::{ProcessSelector, ProcessSort};

/// Run the per-process detail query channel until the session closes.
///
/// `key_prefix` is the sensor's telemetry prefix (e.g. `zensight/sysinfo`) and
/// `hostname` the source segment, so the queryable lives at
/// `<key_prefix>/<hostname>/@/query/processes`.
pub async fn run(session: Arc<zenoh::Session>, key_prefix: String, hostname: String) {
    let key = format!("{key_prefix}/{hostname}/@/query/processes");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare processes failed");
            return;
        }
    };
    tracing::info!(key = %key, "per-process detail query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let sel = ProcessSelector::parse(query.parameters().as_str());
        // The per-pid /proc walk is blocking — keep it off the runtime thread.
        let records = match tokio::task::spawn_blocking(move || collect_processes(sel)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "query: process walk task failed");
                Vec::new()
            }
        };
        reply_json(&query, &records).await;
    }
}

/// Serialize `records` as JSON and reply on the query's own key.
async fn reply_json<T: serde::Serialize>(query: &zenoh::query::Query, records: &T) {
    match serde_json::to_vec(records) {
        Ok(payload) => {
            if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                tracing::warn!(error = %e, "query: reply failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "query: serialize failed"),
    }
}

/// Build a fresh `System`, snapshot every process, rank by the selector, and
/// return the top-N as wire DTOs. Blocking — call under `spawn_blocking`.
fn collect_processes(sel: ProcessSelector) -> Vec<ProcessRecord> {
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
        let recs = collect_processes(sel);
        assert!(recs.len() <= 5);
        // Sorted descending by rss.
        for w in recs.windows(2) {
            assert!(w[0].rss >= w[1].rss);
        }
    }
}
