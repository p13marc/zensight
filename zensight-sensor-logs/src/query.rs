//! On-demand per-line log-event query channel (#358, principle P2).
//!
//! Per-line log events are high-cardinality, high-volume detail; streaming
//! them per line rode (and could dominate) the `zensight/**` telemetry bus.
//! They now live in a bounded in-memory ring served via a Zenoh queryable at
//! `zensight/logs/@/query/events` — pulled by the GUI on open + a slow refresh
//! tick, never streamed. The low-rate rollups (`logs/by_severity/*`,
//! `logs/by_unit/*`, …) stay on the bus for charts/alerts.
//!
//! Selector parameters (zenoh `Parameters`, `;`-separated — e.g.
//! `…/@/query/events?since=1719999000000;max=500`):
//! - `since=<epoch_ms>` — only records with `ts >= since` (inclusive);
//! - `max=<n>` — reply cap (default 500, clamped to the ring);
//! - `host=<name>` — only records from one originating host.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use zensight_common::LogRecord;

/// Default reply cap when no `?max=` selector is supplied.
pub const DEFAULT_EVENTS_REPLY_MAX: usize = 500;

/// Minimum ring capacity (config values below this are clamped up).
pub const MIN_EVENTS_RING_CAPACITY: usize = 100;

/// The bounded ring of recent per-line log events, shared between the intake
/// loop (producer) and the queryable task (consumer).
pub type EventRing = Arc<Mutex<VecDeque<LogRecord>>>;

/// Create an empty ring for `capacity` records (clamped to
/// [`MIN_EVENTS_RING_CAPACITY`]).
pub fn new_ring(capacity: usize) -> (EventRing, usize) {
    let capacity = capacity.max(MIN_EVENTS_RING_CAPACITY);
    (
        Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
        capacity,
    )
}

/// Append one record, evicting the oldest past `capacity`.
pub fn push(ring: &EventRing, capacity: usize, record: LogRecord) {
    if let Ok(mut r) = ring.lock() {
        r.push_back(record);
        while r.len() > capacity {
            r.pop_front();
        }
    }
}

/// Pure reply builder: newest-first, `since`/`host` filtered, capped at `max`.
fn filter_ring(
    records: &VecDeque<LogRecord>,
    since: Option<i64>,
    host: Option<&str>,
    max: usize,
) -> Vec<LogRecord> {
    records
        .iter()
        .rev()
        .filter(|r| since.is_none_or(|s| r.ts >= s))
        .filter(|r| host.is_none_or(|h| r.host == h))
        .take(max)
        .cloned()
        .collect()
}

/// Run the log-event query channel until the session closes. Replies with
/// filtered records (most-recent first) as JSON `Vec<LogRecord>`.
pub async fn run_events(session: Arc<zenoh::Session>, key_prefix: String, ring: EventRing) {
    let key = zensight_common::command::query_key(&key_prefix, "events");
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare events failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand log-event query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let params = query.parameters();
        let since = params.get("since").and_then(|v| v.parse::<i64>().ok());
        let host = params.get("host").map(str::to_string);
        let max = params
            .get("max")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_EVENTS_REPLY_MAX);

        // Snapshot under the lock, reply outside it.
        let records: Vec<LogRecord> = {
            match ring.lock() {
                Ok(r) => filter_ring(&r, since, host.as_deref(), max),
                Err(_) => Vec::new(),
            }
        };
        match serde_json::to_vec(&records) {
            Ok(payload) => {
                if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                    tracing::warn!(error = %e, "query: events reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "query: events serialize failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(uid: &str, ts: i64, host: &str, message: &str) -> LogRecord {
        LogRecord {
            uid: uid.to_string(),
            ts,
            host: host.to_string(),
            facility: "daemon".to_string(),
            severity: "info".to_string(),
            severity_number: 9,
            app: None,
            pid: None,
            message: message.to_string(),
            labels: Default::default(),
        }
    }

    #[test]
    fn filter_is_newest_first_and_since_inclusive() {
        let ring: VecDeque<LogRecord> = (0..5)
            .map(|i| rec(&format!("u{i}"), 100 + i, "web01", "m"))
            .collect();
        let out = filter_ring(&ring, Some(102), None, 100);
        assert_eq!(
            out.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![104, 103, 102],
            "since is an inclusive lower bound, newest first"
        );
    }

    #[test]
    fn filter_caps_at_max_and_honors_host() {
        let mut ring: VecDeque<LogRecord> = VecDeque::new();
        for i in 0..10 {
            let host = if i % 2 == 0 { "web01" } else { "db01" };
            ring.push_back(rec(&format!("u{i}"), i, host, "m"));
        }
        let out = filter_ring(&ring, None, Some("web01"), 3);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|r| r.host == "web01"));
        assert_eq!(out[0].ts, 8, "newest matching first");
    }

    /// Live round-trip: a single-session zenoh get against the running
    /// queryable returns the ring's records with selectors applied (#358).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_queryable_round_trip() {
        // Unique prefix so parallel test runs don't cross-talk.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("test_{nanos}/logs");

        let session = Arc::new(
            zenoh::open(zenoh::Config::default())
                .await
                .expect("open zenoh session"),
        );

        let (ring, capacity) = new_ring(1000);
        for i in 0..5 {
            push(
                &ring,
                capacity,
                rec(&format!("u{i}"), 100 + i, "web01", "m"),
            );
        }
        tokio::spawn(run_events(session.clone(), prefix.clone(), ring));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // zenoh selector params are `;`-separated (Parameters), not `&`.
        let selector = format!("{prefix}/@/query/events?since=102;max=2");
        let replies = session
            .get(&selector)
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get events");
        let reply = replies.recv_async().await.expect("one reply");
        let sample = reply.result().expect("ok reply");
        let records: Vec<LogRecord> =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("decode Vec<LogRecord>");
        assert_eq!(
            records.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![104, 103],
            "newest-first, since inclusive, capped at max=2"
        );
    }

    #[test]
    fn push_evicts_oldest_past_capacity() {
        let (ring, capacity) = new_ring(0); // clamps to MIN_EVENTS_RING_CAPACITY
        assert_eq!(capacity, MIN_EVENTS_RING_CAPACITY);
        for i in 0..(capacity + 10) {
            push(&ring, capacity, rec(&format!("u{i}"), i as i64, "h", "m"));
        }
        let r = ring.lock().unwrap();
        assert_eq!(r.len(), capacity);
        assert_eq!(r.front().unwrap().ts, 10, "oldest ten evicted");
    }
}
